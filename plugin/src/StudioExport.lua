--[[

StudioExport

Captures live Studio content without changing instances or flushing sync writes.
Whole-place exports are explicitly incomplete drafts, not native Save As files.

--]]

const Packages = script.Parent.Parent.Packages
const Http = require(Packages.Http)
const Promise = require(Packages.Promise)
const RbxDom = require(Packages.RbxDom)
const ReflectionDatabase = require(Packages.RbxDom.database)
const encodeProperty = require("./ChangeBatcher/encodeProperty")
const StudioSyncback = require("./StudioSyncback")
const Types = require("./Types")

-- Service and NotCreatable tags alone cannot distinguish Lighting from internal
-- services. Restrict drafts to known place containers, then verify reflection tags.
const PLACE_SERVICES = {
	Chat = true,
	CollectionService = true,
	Lighting = true,
	LocalizationService = true,
	MaterialService = true,
	Players = true,
	ReplicatedFirst = true,
	ReplicatedStorage = true,
	ServerScriptService = true,
	ServerStorage = true,
	SoundService = true,
	StarterGui = true,
	StarterPack = true,
	StarterPlayer = true,
	Teams = true,
	TextChatService = true,
	Workspace = true,
}
const StudioExport = {}

const function hasTag(className, tag)
	const class = ReflectionDatabase.Classes[className]
	return class ~= nil and table.find(class.Tags, tag) ~= nil
end

function StudioExport.resolvePath(root, path)
	assert(type(path) == "string", "Export paths must be strings.")
	if string.sub(path, 1, 5) == "game/" then
		path = string.sub(path, 6)
	end
	assert(path ~= "" and path ~= "game", "An export path must name an instance below game.")
	local node = root
	for _, segment in string.split(path, "/") do
		assert(segment ~= "", `Invalid export path "{path}".`)
		local found = nil
		for _, child in node:GetChildren() do
			if child.Name == segment then
				assert(found == nil, `Ambiguous export path "{path}" at "{segment}".`)
				found = child
			end
		end
		assert(found ~= nil, `Missing export path "{path}" at "{segment}".`)
		node = found
	end
	return node
end

function StudioExport.normalizeRoots(instances)
	assert(type(instances) == "table", "Export roots must be an array.")
	const chosen = {}
	for _, instance in instances do
		chosen[instance] = true
	end
	const roots = {}
	const emitted = {}
	for _, instance in instances do
		local parent = instance.Parent
		while parent ~= nil and not chosen[parent] do
			parent = parent.Parent
		end
		if parent == nil and not emitted[instance] then
			emitted[instance] = true
			table.insert(roots, instance)
		end
	end
	return roots
end

-- Count exactly the archivable traversal expected from native serialization.
-- Skipped ancestors also exclude their entire subtree.
const function subtreeSize(instance, warnings)
	if not instance.Archivable then
		table.insert(warnings, `Excluded "{instance:GetFullName()}" (Archivable is false).`)
		return 0
	end
	local count = 1
	for _, child in instance:GetChildren() do
		count += subtreeSize(child, warnings)
	end
	return count
end

function StudioExport.serviceProperties(instance, warnings)
	const names = {}
	local class = ReflectionDatabase.Classes[instance.ClassName]
	while class ~= nil do
		for name in class.Properties do
			names[name] = true
		end
		class = ReflectionDatabase.Classes[class.Superclass]
	end
	const sortedNames = {}
	for name in names do
		table.insert(sortedNames, name)
	end
	table.sort(sortedNames)
	-- Keep even an otherwise empty property map unambiguously a msgpack map.
	const properties = { Name = { String = instance.Name } }
	for _, name in sortedNames do
		if name == "Name" or name == "Parent" then
			continue
		end
		const descriptor = RbxDom.findCanonicalPropertyDescriptor(instance.ClassName, name)
		if descriptor == nil then
			continue
		end
		if properties[descriptor.name] ~= nil then
			continue
		end
		const canonical = ReflectionDatabase.Classes[descriptor.className].Properties[descriptor.name]
		if canonical.Kind.Canonical.Serialization == "DoesNotSerialize" then
			continue
		end
		if
			descriptor.dataType == "Ref"
			or (descriptor.scriptability ~= "ReadWrite" and descriptor.scriptability ~= "Custom")
		then
			table.insert(
				warnings,
				`Omitted service property "{instance.Name}.{name}" (reference or inaccessible property).`
			)
			continue
		end
		const ok, encoded, value = pcall(encodeProperty, instance, descriptor.name, descriptor, nil)
		if ok and encoded then
			properties[descriptor.name] = value
		else
			table.insert(warnings, `Omitted unreadable or unsupported service property "{instance.Name}.{name}".`)
		end
	end
	return properties
end

-- Dependencies keep capture and transport testable without native serialization.
function StudioExport.capture(request, dependencies)
	assert(Types.ExportPacket(request))
	assert(type(dependencies) == "table", "Capture dependencies must be a table.")
	assert(not dependencies.isRunning(), "Studio export is only available in edit mode, not play mode.")
	const instances = {}
	const roots = {}
	const services = {}
	const warnings = {}
	const function addRoot(instance, service)
		const size = subtreeSize(instance, warnings)
		if size == 0 then
			return
		end
		table.insert(instances, instance)
		table.insert(roots, {
			name = instance.Name,
			className = instance.ClassName,
			service = service,
			descendantCount = size - 1,
		})
	end
	if request.scope == "place" then
		assert(
			request.allowIncomplete,
			"Whole-place draft export requires allowIncomplete. This is not native Save As."
		)
		table.insert(
			warnings,
			"Draft whole-place export is not native Save As. Service-reference properties, unsupported or unreadable properties, and internal services may be omitted."
		)
		const children = dependencies.root:GetChildren()
		table.sort(children, function(a, b)
			return a.Name < b.Name
		end)
		const serviceNames = {}
		for _, service in children do
			if PLACE_SERVICES[service.ClassName] and hasTag(service.ClassName, "Service") then
				assert(not serviceNames[service.Name], `Duplicate service name "{service.Name}".`)
				serviceNames[service.Name] = true
				table.insert(services, {
					name = service.Name,
					className = service.ClassName,
					properties = dependencies.serviceProperties(service, warnings),
				})
				for _, child in service:GetChildren() do
					addRoot(child, service.Name)
				end
			elseif
				ReflectionDatabase.Classes[service.ClassName] ~= nil
				and not hasTag(service.ClassName, "Service")
				and not hasTag(service.ClassName, "NotCreatable")
			then
				addRoot(service, nil)
			else
				table.insert(warnings, `Excluded internal or unsupported service "{service.Name}".`)
			end
		end
	else
		const selected = {}
		for _, path in request.paths do
			table.insert(selected, StudioExport.resolvePath(dependencies.root, path))
		end
		if request.selection then
			for _, instance in dependencies.selection() do
				table.insert(selected, instance)
			end
		end
		assert(#selected > 0, "Model export requires paths or a nonempty Studio selection.")
		-- Validate before normalization so an invalid descendant cannot hide behind
		-- an ancestor selected at the same time.
		for _, instance in selected do
			assert(
				instance ~= dependencies.root
					and not instance:IsA("DataModel")
					and not hasTag(instance.ClassName, "Service"),
				"Services and DataModel cannot be model export roots."
			)
			assert(instance:IsDescendantOf(dependencies.root), "Model export roots must belong to the open place.")
		end
		for _, instance in StudioExport.normalizeRoots(selected) do
			addRoot(instance, nil)
		end
		assert(#instances > 0, "No archivable model roots remain to export.")
		table.insert(warnings, "References outside the chosen model roots may be lost during export.")
	end
	const contents = if #instances == 0 then "" else dependencies.serialize(instances)
	return contents, { roots = roots, services = services, warnings = warnings }
end

const function postChecked(dependencies, url, body)
	const response = dependencies.post(url, body):expect()
	assert(
		response.code >= 200 and response.code < 300,
		`Export HTTP request failed with status {response.code}. {response.body or ""}`
	)
end

function StudioExport.run(apiContext, request, dependencies)
	assert(type(apiContext) == "table", "Export API context must be a table.")
	assert(type(request) == "table", "Export request must be a table.")
	-- A capability must be safe as a single URL segment even for malformed packets.
	assert(type(request.jobId) == "string" and request.jobId:match("^[%w_-]+$"), "Invalid export job capability.")
	const boundary = dependencies
		or {
			isRunning = function()
				return game:GetService("RunService"):IsRunning()
			end,
			post = Http.post,
			root = game,
			selection = function()
				return game:GetService("Selection"):Get()
			end,
			serialize = function(instances)
				return buffer.tostring(game:GetService("SerializationService"):SerializeInstancesAsync(instances))
			end,
			serviceProperties = StudioExport.serviceProperties,
		}
	return Promise.new(function(resolve, reject)
		const baseUrl = apiContext:getBaseUrl()
		const finishUrl = `{baseUrl}/api/export/finish/{request.jobId}`
		const ok, result = pcall(function()
			const contents, manifest = StudioExport.capture(request, boundary)
			const ranges = StudioSyncback.partRanges(#contents, request.partSize)
			for index, range in ranges do
				postChecked(
					boundary,
					`{baseUrl}/api/export/part/{request.jobId}/{index - 1}`,
					string.sub(contents, range[1], range[2])
				)
			end
			manifest.partCount = #ranges
			postChecked(boundary, finishUrl, Http.msgpackEncode(manifest))
		end)
		if ok then
			resolve()
			return
		end
		const message = StudioSyncback.describeError(result)
		const reported, reportError = pcall(
			postChecked,
			boundary,
			finishUrl,
			Http.msgpackEncode({
				partCount = 0,
				roots = {},
				services = {},
				warnings = {},
				error = message,
			})
		)
		reject(if reported then message else `{message} Failure reporting also failed. {tostring(reportError)}`)
	end)
end

return StudioExport
