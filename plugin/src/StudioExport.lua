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
const encodeCollisionGroups = require("./encodeCollisionGroups")
const encodePropertyValue = require("./encodePropertyValue")
const StudioSyncback = require("./StudioSyncback")
const Types = require("./Types")

-- Service and NotCreatable tags alone cannot distinguish Lighting from internal
-- services. Restrict drafts to known place containers, then verify reflection tags.
const PLACE_SERVICES = {
	AssetService = true,
	AvatarSettings = true,
	CSGDictionaryService = true,
	Chat = true,
	CollectionService = true,
	ConfigureServerService = true,
	ContextActionService = true,
	CookiesService = true,
	Debris = true,
	GamePassService = true,
	GuidRegistryService = true,
	HttpService = true,
	InsertService = true,
	Lighting = true,
	LocalizationService = true,
	LodDataService = true,
	LuaWebService = true,
	MaterialService = true,
	NonReplicatedCSGDictionaryService = true,
	Packages = true,
	PermissionsService = true,
	PhysicsService = true,
	PlayerEmulatorService = true,
	Players = true,
	ProximityPromptService = true,
	ReplicatedFirst = true,
	ReplicatedStorage = true,
	ScriptService = true,
	Selection = true,
	SerializationService = true,
	ServerScriptService = true,
	ServerStorage = true,
	ServiceVisibilityService = true,
	SoundService = true,
	StarterGui = true,
	StarterPack = true,
	StarterPlayer = true,
	StudioData = true,
	Teams = true,
	TeleportService = true,
	TestService = true,
	TextChatService = true,
	TimerService = true,
	TouchInputService = true,
	TweenService = true,
	UGCAvatarService = true,
	VRService = true,
	VideoCaptureService = true,
	VideoService = true,
	VoiceChatService = true,
	Workspace = true,
}
const BASE_MATERIAL_PROPERTIES = {}
for _, material in Enum.Material:GetEnumItems() do
	BASE_MATERIAL_PROPERTIES[`{material.Name}Name`] = material
end
-- These readable persisted properties postdate the pinned reflection database.
-- Read their current enum values directly, never through deprecated enum aliases.
const EXTRA_SERVICE_PROPERTIES = {
	Lighting = { LightAttenuationMode = "Enum" },
	Workspace = { NextGenerationPhysicsSolver = "Enum" },
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
	const extraProperties = EXTRA_SERVICE_PROPERTIES[instance.ClassName] or {}
	for name in extraProperties do
		names[name] = true
	end
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
	const references = {}
	const attempted = {}
	for _, name in sortedNames do
		if name == "Name" or name == "Parent" then
			continue
		end
		local descriptor = RbxDom.findCanonicalPropertyDescriptor(instance.ClassName, name)
		if descriptor == nil and extraProperties[name] ~= nil then
			descriptor = {
				className = instance.ClassName,
				dataType = extraProperties[name],
				name = name,
			}
		end
		if descriptor == nil then
			continue
		end
		if attempted[descriptor.name] then
			continue
		end
		attempted[descriptor.name] = true
		const canonical = ReflectionDatabase.Classes[descriptor.className].Properties[descriptor.name]
		if canonical ~= nil and canonical.Kind.Canonical.Serialization == "DoesNotSerialize" then
			continue
		end
		local readable, raw
		if instance.ClassName == "MaterialService" and BASE_MATERIAL_PROPERTIES[descriptor.name] ~= nil then
			-- Serialized override names are hidden, but their public getter returns
			-- the actual authored names without reading protected property fields.
			readable, raw = pcall(function()
				return instance:GetBaseMaterialOverride(BASE_MATERIAL_PROPERTIES[descriptor.name])
			end)
		elseif instance.ClassName == "Workspace" and descriptor.name == "CollisionGroupData" then
			readable, raw = pcall(function()
				const physics = instance.Parent and instance.Parent:FindFirstChildOfClass("PhysicsService")
				assert(physics, "Collision-group capture requires an existing PhysicsService.")
				const groups = {}
				for _, group in physics:GetRegisteredCollisionGroups() do
					-- Native parts retain numeric IDs, including gaps. The name-only
					-- replacement API cannot recover those serialized identities.
					table.insert(groups, {
						Id = physics:GetCollisionGroupId(group.name),
						Mask = group.mask,
						Name = group.name,
					})
				end
				return encodeCollisionGroups(groups)
			end)
		elseif descriptor.scriptability == "Custom" then
			const ok, success, value = pcall(descriptor.read, descriptor, instance)
			readable = ok and success
			raw = if ok then value else success
		else
			-- Reflection scriptability can lag Studio. The ordinary getter still
			-- enforces the caller's permissions, including read-only properties.
			readable, raw = pcall(function()
				return instance[descriptor.name]
			end)
		end
		if not readable then
			table.insert(
				warnings,
				`Omitted unreadable service property "{instance.Name}.{descriptor.name}". {tostring(raw)}`
			)
			if instance.ClassName == "Workspace" and descriptor.name == "NeedsPivotMigration" then
				table.insert(
					warnings,
					'Unavailable "Workspace.NeedsPivotMigration" can cause Studio to clear legacy non-descendant PrimaryPart references when reopening this draft. Its value was not inferred.'
				)
			end
			continue
		end
		if descriptor.dataType == "Ref" then
			if raw == nil then
				properties[descriptor.name] = { Ref = "00000000000000000000000000000000" }
			else
				table.insert(references, { property = descriptor.name, target = raw })
			end
			continue
		end
		const ok, encoded, value = pcall(encodePropertyValue, raw, descriptor.dataType)
		if not ok or not encoded then
			const reason = if ok then value else encoded
			table.insert(
				warnings,
				`Omitted unsupported service property "{instance.Name}.{descriptor.name}". {tostring(reason)}`
			)
			continue
		end
		if descriptor.dataType == "Attributes" then
			for attributeName in raw do
				if value.Attributes[attributeName] == nil then
					table.insert(warnings, `Omitted unsupported service attribute "{instance.Name}.{attributeName}".`)
				end
			end
		end
		properties[descriptor.name] = value
	end
	return properties, references
end

const function warnExternalReferences(roots, warnings)
	const included = {}
	const pending = table.clone(roots)
	while #pending > 0 do
		const instance = table.remove(pending)
		if instance.Archivable then
			included[instance] = true
			for _, child in instance:GetChildren() do
				table.insert(pending, child)
			end
		end
	end
	const referenceProperties = {}
	for instance in included do
		local names = referenceProperties[instance.ClassName]
		if names == nil then
			names = {}
			local class = ReflectionDatabase.Classes[instance.ClassName]
			while class ~= nil do
				for name in class.Properties do
					const descriptor = RbxDom.findCanonicalPropertyDescriptor(instance.ClassName, name)
					if descriptor ~= nil and descriptor.dataType == "Ref" and descriptor.name ~= "Parent" then
						const canonical = ReflectionDatabase.Classes[descriptor.className].Properties[descriptor.name]
						if canonical.Kind.Canonical.Serialization ~= "DoesNotSerialize" then
							names[descriptor.name] = true
						end
					end
				end
				class = ReflectionDatabase.Classes[class.Superclass]
			end
			referenceProperties[instance.ClassName] = names
		end
		for name in names do
			const ok, target = pcall(function()
				return instance[name]
			end)
			if ok and target ~= nil and not included[target] then
				table.insert(
					warnings,
					`External reference "{instance:GetFullName()}.{name}" is outside the captured model.`
				)
			end
		end
	end
end

-- Dependencies keep capture and transport testable without native serialization.
function StudioExport.capture(request, dependencies)
	assert(Types.ExportPacket(request))
	assert(type(dependencies) == "table", "Capture dependencies must be a table.")
	assert(not dependencies.isRunning(), "Studio export is only available in edit mode, not play mode.")
	const instances = {}
	const roots = {}
	const services = {}
	const rootIndices = {}
	const serviceIndices = {}
	const pendingReferences = {}
	const warnings = {}
	const function addRoot(instance, service)
		const size = subtreeSize(instance, warnings)
		if size == 0 then
			return
		end
		rootIndices[instance] = #instances
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
				serviceIndices[service] = #services
				const properties, references = dependencies.serviceProperties(service, warnings)
				table.insert(pendingReferences, references or {})
				table.insert(services, {
					name = service.Name,
					className = service.ClassName,
					properties = properties,
					references = {},
				})
				for _, child in service:GetChildren() do
					-- Studio omits runtime players even when Archivable is true, so they
					-- must not contribute roots to the reconstruction manifest.
					if child.ClassName == "Player" then
						table.insert(
							warnings,
							`Excluded runtime player "{child:GetFullName()}" (not serialized by Studio).`
						)
						continue
					end
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
		warnExternalReferences(instances, warnings)
	end
	-- Resolve only after every service and native root has an identity. Wire indexes are
	-- zero-based positions, never paths or names that can alias different instances.
	for serviceIndex, references in pendingReferences do
		const service = services[serviceIndex]
		for _, reference in references do
			const rootIndex = rootIndices[reference.target]
			const targetServiceIndex = serviceIndices[reference.target]
			local target
			if rootIndex ~= nil then
				target = { kind = "root", index = rootIndex }
			elseif targetServiceIndex ~= nil then
				target = { kind = "service", index = targetServiceIndex }
			else
				table.insert(
					warnings,
					`Omitted service reference property "{service.name}.{reference.property}" (target is not a captured root or service).`
				)
				continue
			end
			table.insert(service.references, { property = reference.property, target = target })
		end
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
