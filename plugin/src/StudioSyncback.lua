--[[
	Sends the open place to `rojo syncback --from-studio`.

	Studio can't serialize services, so the children of every service the
	project maps are serialized in one SerializeInstancesAsync call. One call
	keeps references between services intact, which separate calls would turn
	into nil. The buffer is uploaded in parts, several at a time, followed by a
	manifest that tells the server which service each child belongs to.
]]

const HttpService = game:GetService("HttpService")
const SerializationService = game:GetService("SerializationService")

const Packages = script.Parent.Parent.Packages
const Http = require(Packages.Http)
const Promise = require(Packages.Promise)

const Config = require(script.Parent.Config)

-- Uploads overlap with each other, which roughly triples throughput on large
-- places compared to sending parts one at a time.
const PARALLEL_UPLOADS = 4

const StudioSyncback = {}

--[[
	Collects the archivable children of each service, in order, along with the
	manifest describing them. `findService` returns the place's instance for a
	name and class name, or nil when the place doesn't have one. Mappings with no
	instance are returned as missing.
]]
function StudioSyncback.collect(services, findService)
	const instances = {}
	const manifest = {}
	const missing = {}

	for _, service in services do
		const instance = findService(service.name, service.className)
		if instance == nil then
			table.insert(missing, service.name)
			continue
		end

		for _, child in instance:GetChildren() do
			if child.Archivable then
				table.insert(instances, child)
				table.insert(manifest, {
					service = service.name,
					name = child.Name,
					className = child.ClassName,
				})
			end
		end
	end

	return instances, manifest, missing
end

--[[
	Locates `camera` within the uploaded instances, since the server can't read
	Workspace.CurrentCamera itself. The path starts with the zero-based index of
	the uploaded instance containing the camera, followed by zero-based child
	indices down to it. Only archivable children are counted, because those are
	the only ones Studio serializes. Returns nil when the camera isn't uploaded.
]]
function StudioSyncback.cameraPath(instances, camera)
	const uploadIndex = {}
	for index, instance in instances do
		uploadIndex[instance] = index - 1
	end

	const path = {}
	local node = camera
	while node ~= nil and uploadIndex[node] == nil do
		const parent = node.Parent
		if parent == nil or not node.Archivable then
			return nil
		end

		local childIndex = 0
		for _, sibling in parent:GetChildren() do
			if sibling == node then
				break
			end
			if sibling.Archivable then
				childIndex += 1
			end
		end
		table.insert(path, 1, childIndex)
		node = parent
	end

	if node == nil or not node.Archivable then
		return nil
	end
	table.insert(path, 1, uploadIndex[node])
	return path
end

--[[
	Splits `size` bytes into inclusive { first, last } ranges of at most
	`partSize` bytes, for use with string.sub.
]]
function StudioSyncback.partRanges(size, partSize)
	const ranges = {}
	for first = 1, size, partSize do
		table.insert(ranges, { first, math.min(first + partSize - 1, size) })
	end
	return ranges
end

--[[
	Builds the server URL, putting IPv6 addresses in brackets so the port isn't
	read as part of the address.
]]
function StudioSyncback.baseUrl(host, port)
	if string.find(host, ":", 1, true) and string.sub(host, 1, 1) ~= "[" then
		host = `[{host}]`
	end
	return `http://{host}:{port}`
end

--[[
	Turns a failed request into a readable message, using the details the server
	sends back when there are any.
]]
function StudioSyncback.describeError(err)
	if type(err) == "table" and type(err.details) == "string" then
		return err.details
	end
	return tostring(err)
end

--[[
	Describes mappings the place doesn't have. Sending them as empty would make
	syncback remove their files, so the upload is refused instead.
]]
function StudioSyncback.describeMissing(missing)
	const names = {}
	for _, name in missing do
		table.insert(names, `"{name}"`)
	end
	const pronoun = if #missing == 1 then "it" else "them"
	return `The project maps {table.concat(names, ", ")}, but this place doesn't have {pronoun}, so `
		.. `nothing was sent. Syncing back would remove {if #missing == 1 then "its" else "their"} files.`
end

-- Projects map the DataModel's children by name, which covers services and any
-- other top-level instance, like a Folder.
local function findService(name, className)
	for _, child in game:GetChildren() do
		const ok, matches = pcall(function()
			return child.Name == name and child.ClassName == className
		end)
		if ok and matches then
			return child
		end
	end
	return nil
end

local function uploadParts(baseUrl, syncbackId, uploadId, contents, ranges)
	local cursor = 0

	-- Each worker uploads one part at a time until none are left.
	local function worker()
		cursor += 1
		const index = cursor
		if index > #ranges then
			return Promise.resolve()
		end

		const range = ranges[index]
		const url = `{baseUrl}/api/syncback/part/{syncbackId}/{uploadId}/{index - 1}`
		return Http.post(url, string.sub(contents, range[1], range[2])):andThen(worker)
	end

	const workers = {}
	for _ = 1, math.min(PARALLEL_UPLOADS, #ranges) do
		table.insert(workers, worker())
	end
	return Promise.all(workers)
end

--[[
	Uploads the open place to the syncback server at `baseUrl`, resolving with
	what the server received.
]]
function StudioSyncback.run(baseUrl)
	return Http.get(baseUrl .. "/api/syncback")
		:andThen(function(response)
			const info = response:msgpack()
			if info.protocolVersion ~= Config.protocolVersion then
				return Promise.reject(
					`The Rojo server uses protocol version {info.protocolVersion}, but this plugin `
						.. `uses {Config.protocolVersion}. Make sure the plugin and server versions match.`
				)
			end

			const instances, manifest, missing = StudioSyncback.collect(info.services, findService)
			if #missing > 0 then
				return Promise.reject(StudioSyncback.describeMissing(missing))
			end
			const cameraPath = StudioSyncback.cameraPath(instances, workspace.CurrentCamera)
			-- A fresh ID per attempt keeps parts from an earlier failed attempt out
			-- of this one.
			const uploadId = HttpService:GenerateGUID(false)

			const startTime = os.clock()
			local contents, serializeTime, ranges

			-- SerializeInstancesAsync yields, which a Promise executor is allowed to do.
			return Promise.new(function(resolve)
				-- An empty upload is still sent, so syncback can remove what the
				-- emptied services used to contain.
				contents = if #instances == 0
					then ""
					else buffer.tostring(SerializationService:SerializeInstancesAsync(instances))
				serializeTime = os.clock() - startTime
				ranges = StudioSyncback.partRanges(#contents, info.partSize)
				resolve()
			end)
				:andThen(function()
					return uploadParts(baseUrl, info.syncbackId, uploadId, contents, ranges)
				end)
				:andThen(function()
					return Http.post(
						baseUrl .. "/api/syncback/finish",
						Http.msgpackEncode({
							syncbackId = info.syncbackId,
							uploadId = uploadId,
							partCount = #ranges,
							manifest = manifest,
							currentCamera = cameraPath,
						})
					)
				end)
				:andThen(function(finishResponse)
					const result = finishResponse:msgpack()
					return {
						success = true,
						projectName = info.projectName,
						instanceCount = result.instanceCount,
						dropped = result.dropped,
						bytes = #contents,
						parts = #ranges,
						serializeSeconds = serializeTime,
						totalSeconds = os.clock() - startTime,
					}
				end)
		end)
		:catch(function(err)
			return {
				success = false,
				error = StudioSyncback.describeError(err),
			}
		end)
end

return StudioSyncback
