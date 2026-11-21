local StudioService = game:GetService("StudioService")
local RunService = game:GetService("RunService")
local ChangeHistoryService = game:GetService("ChangeHistoryService")
local SerializationService = game:GetService("SerializationService")
local Selection = game:GetService("Selection")

local Packages = script.Parent.Parent.Packages
local Log = require(Packages.Log)
local Fmt = require(Packages.Fmt)
local Http = require(Packages.Http)
local t = require(Packages.t)
local Promise = require(Packages.Promise)
local Timer = require(script.Parent.Timer)

local ChangeBatcher = require(script.Parent.ChangeBatcher)
local encodePatchUpdate = require(script.Parent.ChangeBatcher.encodePatchUpdate)
local getWritablePropertyDescriptor = require(script.Parent.ChangeBatcher.getWritablePropertyDescriptor)
local InstanceMap = require(script.Parent.InstanceMap)
local PatchSet = require(script.Parent.PatchSet)
local Reconciler = require(script.Parent.Reconciler)
local ReconcilerServices = require(script.Parent.Reconciler.services)
local strict = require(script.Parent.strict)
local Settings = require(script.Parent.Settings)
local StudioExport = require(script.Parent.StudioExport)
local orderSwaps = require(script.Parent.orderSwaps)
local yieldBudget = require(script.Parent.yieldBudget)

local Status = strict("Session.Status", {
	NotStarted = "NotStarted",
	Connecting = "Connecting",
	Connected = "Connected",
	Disconnected = "Disconnected",
})

local function debugPatch(object)
	return Fmt.debugify(object, function(patch, output)
		output:writeLine("Patch {{")
		output:indent()

		for removed in ipairs(patch.removed) do
			output:writeLine("Remove ID {}", removed)
		end

		for id, added in pairs(patch.added) do
			output:writeLine("Add ID {} {:#?}", id, added)
		end

		for _, updated in ipairs(patch.updated) do
			output:writeLine("Update ID {} {:#?}", updated.id, updated)
		end

		output:unindent()
		output:write("}")
	end)
end

local function attemptReparent(instance, parent)
	return pcall(function()
		instance.Parent = parent
	end)
end

local ServeSession = {}
ServeSession.__index = ServeSession

ServeSession.Status = Status

-- Names the undo waypoint for each applied patch. The app watches for it to
-- notice when a patch is undone.
ServeSession.PATCH_RECORDING_PREFIX = "Rojo+: Patch "

-- Raised inside the initial sync when the session is stopped partway through.
ServeSession.SYNC_CANCELLED = "Rojo+ sync was cancelled because the session stopped"

function ServeSession.getWriteStatus(result)
	assert(t.table(result))

	if #result.errors > 0 then
		return "Failed"
	end

	local conflicts = result.conflicts
	if #conflicts.removed_instances > 0 or #conflicts.added_instances > 0 or #conflicts.updated_instances > 0 then
		return "Conflicted"
	end

	return "Saved"
end

function ServeSession.shouldPreserveFailedProperty(result, id, propertyName)
	assert(t.table(result))
	assert(t.string(id))
	assert(t.string(propertyName))

	if propertyName ~= "Source" then
		return false
	end
	local deferred = false
	for _, err in result.errors do
		if err.id == id and err.kind == "sourceSerializationDeferred" and err.property == "Source" then
			deferred = true
			break
		end
	end
	if not deferred then
		return false
	end
	for _, update in result.conflicts.updated_instances do
		if
			update.id == id
			and (
				update.changed_properties.Source ~= nil
				or update.changed_name ~= nil
				or update.changedParent ~= nil
				or update.changed_class_name ~= nil
				or update.changed_metadata ~= nil
			)
		then
			return false
		end
	end
	for _, removedId in result.conflicts.removed_instances do
		if removedId == id then
			return false
		end
	end
	return true
end

local validateServeOptions = t.strictInterface({
	apiContext = t.table,
	twoWaySync = t.boolean,
	-- Pins the authority for this session, so a live edit place always resolves
	-- conflicts in favor of the filesystem without changing the user's setting.
	twoWaySyncAuthority = t.optional(t.string),
})

function ServeSession.new(options)
	assert(validateServeOptions(options))

	-- Declare self ahead of time to capture it in a closure
	local self
	local function onInstanceChanged(instance, propertyName)
		self:__onInstanceChanged(instance, propertyName)
	end

	local function onChildAdded(child)
		self:__onChildAdded(child)
	end

	-- Each batch keeps the cursor it started from, taken before preparing it
	-- yields, since an edit made meanwhile starts the next batch.
	local function onBatchTaken(changes)
		local cursor = self.__pendingWriteCursor
		self.__pendingWriteCursor = nil
		return { cursor = cursor, properties = self:__takeLocalPropertyEdits(changes) }
	end

	local function onBatchPrepared(patch, context)
		self:__finishOmittedLocalProperties(patch, context.properties)
	end

	local function onChangesFlushed(patch, context)
		return self:__writePatch(patch, context.cursor, context.properties)
	end

	local instanceMap = InstanceMap.new(onInstanceChanged, onChildAdded)
	local changeBatcher = ChangeBatcher.new(instanceMap, onChangesFlushed, onBatchTaken, onBatchPrepared)
	local reconciler = Reconciler.new(instanceMap)

	local connections = {}

	local connection = StudioService:GetPropertyChangedSignal("ActiveScript"):Connect(function()
		local activeScript = StudioService.ActiveScript

		if activeScript ~= nil then
			self:__onActiveScriptChanged(activeScript)
		end
	end)
	table.insert(connections, connection)

	self = {
		__status = Status.NotStarted,
		__apiContext = options.apiContext,
		__twoWaySync = options.twoWaySync,
		__twoWaySyncAuthority = options.twoWaySyncAuthority,
		__reconciler = reconciler,
		__instanceMap = instanceMap,
		__changeBatcher = changeBatcher,
		-- Studio edits seen before the initial sync knows the whole place. Nil
		-- once they have been released.
		__deferredChanges = {},
		__statusChangedCallback = nil,
		__connections = connections,
		__syncResults = {},
		__syncResultCallback = nil,
		__localPropertyEdits = {},
		__precommitCallbacks = {},
		__postcommitCallbacks = {},
		__updateLoadingText = function() end,
	}

	setmetatable(self, ServeSession)

	return self
end

function ServeSession:__fmtDebug(output)
	output:writeLine("ServeSession {{")
	output:indent()

	output:writeLine("API Context: {:#?}", self.__apiContext)
	output:writeLine("Instances: {:#?}", self.__instanceMap)

	output:unindent()
	output:write("}")
end

function ServeSession:getStatus()
	return self.__status
end

--[[
	The initial sync yields while it walks the place, so Studio edits can arrive
	while only part of it is known. Sending one then would judge it against a
	half-built instance map, where a move into a folder not reached yet looks
	like a move to an unmanaged parent and gets undone. They are held instead.
]]
function ServeSession:__onInstanceChanged(instance, propertyName)
	if not self.__twoWaySync then
		return
	end

	local deferred = self.__deferredChanges
	if deferred ~= nil then
		table.insert(deferred, { instance = instance, propertyName = propertyName })
		return
	end

	self:__queueChange(instance, propertyName)
end

-- An instance appeared under a known parent, which is how instances made in
-- Studio show up. Like edits, they are held during the initial sync.
function ServeSession:__onChildAdded(child)
	if not self.__twoWaySync then
		return
	end

	local deferred = self.__deferredChanges
	if deferred ~= nil then
		table.insert(deferred, { child = child })
		return
	end

	self:__markBatchStart()
	self.__changeBatcher:addChild(child)
end

-- Sends the held edits, in the order they happened, now that every instance
-- they could refer to is known.
function ServeSession:__releaseDeferredChanges()
	local deferred = self.__deferredChanges
	if deferred == nil then
		return
	end
	self.__deferredChanges = nil

	for _, change in deferred do
		if change.child ~= nil then
			self:__markBatchStart()
			self.__changeBatcher:addChild(change.child)
		else
			self:__queueChange(change.instance, change.propertyName)
		end
	end
end

-- Retain the revision preceding the first local change, even if a websocket
-- update arrives before this batch is sent.
function ServeSession:__markBatchStart()
	if self.__changeBatcher:isEmpty() then
		self.__pendingWriteCursor = self.__apiContext:getMessageCursor()
	end
end

-- Collects every id an addition gave out, root first.
local function collectAddedIds(entry, out)
	table.insert(out, entry.id)
	for _, child in entry.children do
		collectAddedIds(child, out)
	end
	return out
end

--[[
	Stops tracking instances Studio created that the server didn't store. They
	stay in Studio, but nothing refers to ids the server never took.
]]
function ServeSession:__untrackRejectedAdditions(patch, result)
	if next(patch.added) == nil then
		return
	end

	local rejected = {}
	if result == nil then
		for _, addition in patch.added do
			for _, id in collectAddedIds(addition.ids, {}) do
				rejected[id] = true
			end
		end
	else
		for _, err in result.errors or {} do
			-- An error about one property of a saved instance, like a reference
			-- that couldn't be stored, leaves the instance saved and tracked.
			if err.property ~= nil then
				continue
			end
			local addition = patch.added[err.id]
			if addition ~= nil then
				for _, id in collectAddedIds(addition.ids, {}) do
					rejected[id] = true
				end
			else
				rejected[err.id] = true
			end
		end
	end

	for _, addition in patch.added do
		for _, id in collectAddedIds(addition.ids, {}) do
			if rejected[id] then
				self.__instanceMap:removeId(id)
			end
		end
	end
end

--[[
	Builds the function the initial sync calls as it walks the place. `budget`
	yields when it is due and reports whether it did. A session stopped during
	that yield has had its instance map cleared, so the walk must not touch it
	again, and unwinding with an error is what stops it.
]]
function ServeSession:__syncStep(budget)
	return function()
		if budget() and self.__status == Status.Disconnected then
			error(ServeSession.SYNC_CANCELLED, 0)
		end
	end
end

--[[
	Stops tracking instances whose deletion the server saved, with their
	descendants. Their files are gone, so if an undo brings them back, they are
	new instances to the server and must be sent as created.
]]
function ServeSession:__untrackSavedRemovals(result)
	local restored = {}
	for _, id in result.accepted.removed_instances do
		local instance = self.__instanceMap.fromIds[id]
		if instance ~= nil then
			for _, descendant in instance:GetDescendants() do
				self.__instanceMap:removeInstance(descendant)
			end
			table.insert(restored, instance)
		end
		self.__instanceMap:removeId(id)
	end
	-- An undo before this response brought an instance back while it was
	-- still tracked, so nothing queued it. It's created again now.
	for _, instance in restored do
		local parent = instance.Parent
		if parent ~= nil and self.__instanceMap.fromInstances[parent] ~= nil then
			self:__markBatchStart()
			self.__changeBatcher:addChild(instance)
		end
	end
end

--[[
	The server announces a deletion this session saved, and the announcement
	can arrive before the write's response. If an undo brought the instance
	back meanwhile, removing it again would undo the undo, so it's left out of
	the patch, stops being tracked, and is created again.
]]
function ServeSession:__keepUndoneDeletions(patch)
	local pending = self.__pendingDeletions
	if pending == nil or #patch.removed == 0 then
		return
	end
	local removed = {}
	local undone = {}
	for _, id in patch.removed do
		local instance = pending[id]
		if instance ~= nil and instance.Parent ~= nil then
			table.insert(undone, instance)
			pending[id] = nil
		else
			table.insert(removed, id)
		end
	end
	if #undone == 0 then
		return
	end
	patch.removed = removed
	for _, instance in undone do
		for _, descendant in instance:GetDescendants() do
			self.__instanceMap:removeInstance(descendant)
		end
		self.__instanceMap:removeInstance(instance)
	end
	for _, instance in undone do
		local parent = instance.Parent
		if parent ~= nil and self.__instanceMap.fromInstances[parent] ~= nil then
			self:__markBatchStart()
			self.__changeBatcher:addChild(instance)
		end
	end
end

function ServeSession:__rememberLocalProperty(instance, propertyName)
	local instanceMap = self.__instanceMap
	local id = instanceMap and instanceMap.fromInstances[instance]
	if id == nil or instance:IsA("DataModel") or propertyName == "Name" or propertyName == "Parent" then
		return
	end
	-- Source, Value, and Contents can be file payloads. Buffer their echoes until the
	-- server decides authority, including any rejected-write restoration.
	local descriptor = getWritablePropertyDescriptor(instance.ClassName, propertyName)
	if descriptor == nil or descriptor.dataType == "Ref" then
		return
	end
	self.__localPropertyEdits = self.__localPropertyEdits or {}
	local properties = self.__localPropertyEdits[id] or {}
	self.__localPropertyEdits[id] = properties
	-- Each entry is a generation token. A previous request cannot release a later edit.
	properties[descriptor.name] = {}
end

function ServeSession:__takeLocalPropertyEdits(changes)
	local taken = {}
	for instance, changed in changes do
		local id = self.__instanceMap.fromInstances[instance]
		local properties = self.__localPropertyEdits[id]
		if properties ~= nil then
			local entries = {}
			for propertyName in changed do
				local descriptor = getWritablePropertyDescriptor(instance.ClassName, propertyName)
				if descriptor ~= nil then
					entries[descriptor.name] = properties[descriptor.name]
				end
			end
			taken[id] = entries
		end
	end
	return taken
end

function ServeSession:__finishOmittedLocalProperties(patch, taken)
	local emitted = {}
	if patch ~= nil then
		for _, update in patch.updated do
			emitted[update.id] = update.changedProperties
		end
	end
	local omitted = {}
	for id, entries in taken do
		local properties = emitted[id]
		for propertyName, entry in entries do
			if properties == nil or properties[propertyName] == nil then
				omitted[id] = omitted[id] or {}
				omitted[id][propertyName] = entry
			end
		end
	end
	self:__finishLocalProperties(omitted, nil)
end

function ServeSession:__protectLocalProperties(patch, publicationCursor)
	local edits = self.__localPropertyEdits
	if edits == nil or next(edits) == nil then
		return patch
	end
	local structural = {}
	local replaced = {}
	local removed = {}
	for id in patch.added do
		structural[id] = true
	end
	for _, id in patch.removed do
		structural[id] = true
		replaced[id] = true
		removed[id] = true
	end
	for _, update in patch.updated do
		if
			update.changedName ~= nil
			or update.changedParent ~= nil
			or update.changedClassName ~= nil
			or update.changedMetadata ~= nil
		then
			structural[update.id] = true
		end
		if update.changedClassName ~= nil then
			replaced[update.id] = true
		end
	end
	local filtered = table.clone(patch)
	filtered.updated = {}
	-- Invalidate only the changed subtree. Buffered values must survive unrelated
	-- structure, and a rename must not discard a newer authoritative property value.
	if next(structural) ~= nil then
		local instanceMap = self.__instanceMap
		for id, properties in edits do
			local target = instanceMap.fromIds[id]
			local instance = target
			local invalidated = false
			local discarded = replaced[id] == true
			while instance ~= nil do
				local ancestorId = instanceMap.fromInstances[instance]
				invalidated = invalidated or structural[ancestorId] == true
				-- Class changes retain children, and removals run after moves. Only a
				-- remove-and-add subtree replacement destroys descendants before updates.
				if
					removed[ancestorId]
					and patch.added[ancestorId] ~= nil
					and (target == instance or target:IsDescendantOf(instance))
				then
					discarded = true
				end
				instance = instanceMap.knownParents[instance] or instance.Parent
			end
			if invalidated then
				edits[id] = nil
				local restored = {}
				for propertyName, entry in properties do
					restored[propertyName] = entry.serverValue
				end
				if not discarded and next(restored) ~= nil then
					table.insert(filtered.updated, { id = id, changedProperties = restored })
				end
			end
		end
	end
	for _, update in patch.updated do
		local properties = edits[update.id]
		if properties == nil then
			table.insert(filtered.updated, update)
			continue
		end
		local kept = table.clone(update)
		kept.changedProperties = table.clone(update.changedProperties or {})
		local instance = self.__instanceMap.fromIds[update.id]
		for propertyName, value in kept.changedProperties do
			local descriptor = instance and getWritablePropertyDescriptor(instance.ClassName, propertyName)
			local entry = properties[if descriptor ~= nil then descriptor.name else propertyName]
			if entry ~= nil then
				entry.serverValue = value
				entry.serverCursor = publicationCursor
				kept.changedProperties[propertyName] = nil
			end
		end
		if next(kept.changedProperties) ~= nil then
			table.insert(filtered.updated, kept)
		end
	end
	return filtered
end

function ServeSession:__releaseLocalProperties(cursor)
	local edits = self.__localPropertyEdits
	if edits == nil then
		return
	end
	local patch = PatchSet.newEmpty()
	for id, properties in edits do
		local released = {}
		for propertyName, entry in properties do
			if entry.cursor ~= nil and entry.cursor <= cursor then
				properties[propertyName] = nil
				if not entry.preserveLocal or (entry.serverCursor ~= nil and entry.serverCursor > entry.cursor) then
					released[propertyName] = entry.serverValue
				end
			end
		end
		if next(properties) == nil then
			edits[id] = nil
		end
		if next(released) ~= nil and self.__instanceMap.fromIds[id] ~= nil then
			table.insert(patch.updated, { id = id, changedProperties = released })
		end
	end
	if not PatchSet.isEmpty(patch) then
		self:__applyPatch(patch)
	end
end

function ServeSession:__finishLocalProperties(taken, result)
	if taken == nil or next(taken) == nil or self.__localPropertyEdits == nil then
		return
	end
	local cursor = self.__apiContext:getMessageCursor()
	for id, entries in taken do
		local properties = self.__localPropertyEdits[id]
		if properties == nil then
			continue
		end
		for propertyName, entry in entries do
			if properties[propertyName] == entry then
				-- A failed serialization leaves an unsaved draft, not a disk conflict. Drain
				-- older echoes through the response cursor without replaying them over it.
				entry.preserveLocal = result ~= nil
					and ServeSession.shouldPreserveFailedProperty(result, id, propertyName)
				-- A transport failure or older response cannot leave a permanent guard.
				entry.cursor = if result and result.messageCursor ~= nil then result.messageCursor else cursor
			end
		end
	end
	self:__releaseLocalProperties(cursor)
end

function ServeSession:__queueChange(instance, propertyName)
	self:__rememberLocalProperty(instance, propertyName)
	self:__markBatchStart()
	self.__changeBatcher:add(instance, propertyName)
end

function ServeSession:__writePatch(patch, messageCursor, localProperties)
	-- Read when each batch is sent, so changing it applies to the next write
	-- without reconnecting. A pinned session authority wins over the setting.
	local authority = self.__twoWaySyncAuthority or Settings:get("twoWaySyncAuthority")

	-- One record per write, which moves from Pending to its outcome, so a write
	-- is never reported as both pending and finished.
	local record = {
		status = "Pending",
		patch = patch,
		authority = authority,
	}
	table.insert(self.__syncResults, record)

	local function finish(status, result)
		record.status = status
		record.result = result
		if self.__syncResultCallback ~= nil then
			self.__syncResultCallback(status, result)
		end
	end

	-- The instances this write deletes are remembered until it's answered, so
	-- an announcement of the deletion can tell whether an undo came first.
	self.__pendingDeletions = self.__pendingDeletions or {}
	for _, id in patch.removed do
		if self.__instanceMap ~= nil and self.__instanceMap.fromIds[id] ~= nil then
			self.__pendingDeletions[id] = self.__instanceMap.fromIds[id]
		end
	end
	local function forgetDeletions()
		for _, id in patch.removed do
			self.__pendingDeletions[id] = nil
		end
	end

	-- Writes reach the server in the order they were made, so a later edit to
	-- an instance never arrives before the write that created it. A write
	-- still waiting when the session stops is never sent.
	local previous = self.__lastWrite or Promise.resolve()
	local acknowledged = false
	local write = previous
		:andThen(function()
			if self.__status == Status.Disconnected then
				return Promise.reject("The session stopped before this write was sent")
			end
			return self.__apiContext:write(patch, messageCursor, authority)
		end)
		:andThen(function(result)
			acknowledged = true
			forgetDeletions()
			self:__untrackRejectedAdditions(patch, result)
			self:__untrackSavedRemovals(result)
			finish(ServeSession.getWriteStatus(result), result)
			-- Handlers and callbacks must succeed before buffered values can replay.
			self:__finishLocalProperties(localProperties, result)
			return result
		end)
		:catch(function(err)
			forgetDeletions()
			if acknowledged or (getmetatable(err) == Http.Error and err.type == Http.Error.Kind.Protocol) then
				-- An invalid or unprocessed acknowledgement cannot release guards or
				-- keep tracking edits. Stop before queued writes are considered.
				self:__stopInternal(err)
			else
				self:__finishLocalProperties(localProperties, nil)
				self:__untrackRejectedAdditions(patch, nil)
			end
			finish("Failed", {
				errors = {
					{
						message = tostring(err),
					},
				},
			})
		end)
	self.__lastWrite = write
	return write
end

function ServeSession:getSyncResults()
	local results = {}
	for index, record in self.__syncResults do
		results[index] = table.clone(record)
	end
	return results
end

function ServeSession:flushChanges()
	return self.__changeBatcher:__flush()
end

function ServeSession:setSyncResultCallback(callback)
	self.__syncResultCallback = callback
end

function ServeSession:clearSyncResults()
	table.clear(self.__syncResults)
end

function ServeSession:onStatusChanged(callback)
	self.__statusChangedCallback = callback
end

function ServeSession:setConfirmCallback(callback)
	self.__userConfirmCallback = callback
end

function ServeSession:setUpdateLoadingTextCallback(callback)
	self.__updateLoadingText = callback
end

function ServeSession:setLoadingText(text: string)
	self.__updateLoadingText(text)
end

--[=[
	Hooks a function to run before patch application.
	The provided function is called with the incoming patch and an InstanceMap
	as parameters.
]=]
function ServeSession:hookPrecommit(callback)
	table.insert(self.__precommitCallbacks, callback)
	Log.trace("Added precommit callback: {}", callback)

	return function()
		-- Remove the callback from the list
		for i, cb in self.__precommitCallbacks do
			if cb == callback then
				table.remove(self.__precommitCallbacks, i)
				Log.trace("Removed precommit callback: {}", callback)
				break
			end
		end
	end
end

--[=[
	Hooks a function to run after patch application.
	The provided function is called with the applied patch, the current
	InstanceMap, and a PatchSet containing any unapplied changes.
]=]
function ServeSession:hookPostcommit(callback)
	table.insert(self.__postcommitCallbacks, callback)
	Log.trace("Added postcommit callback: {}", callback)

	return function()
		-- Remove the callback from the list
		for i, cb in self.__postcommitCallbacks do
			if cb == callback then
				table.remove(self.__postcommitCallbacks, i)
				Log.trace("Removed postcommit callback: {}", callback)
				break
			end
		end
	end
end

function ServeSession:start()
	self:__setStatus(Status.Connecting)
	self:setLoadingText("Connecting to server...")

	self.__apiContext
		:connect()
		:andThen(function(serverInfo)
			self:setLoadingText("Loading initial data from server...")
			return self:__initialSync(serverInfo):andThen(function()
				self:setLoadingText("Starting sync loop...")
				self:__setStatus(Status.Connected, serverInfo.projectName)
				self:__applyGameAndPlaceId(serverInfo)

				return self.__apiContext:connectWebSocket({
					["export"] = function(body)
						if self.__status == Status.Disconnected then
							return
						end
						-- Independent capture never flushes or writes syncback patches.
						task.spawn(function()
							StudioExport.run(self.__apiContext, body):catch(function(err)
								Log.warn("Studio export failed. {}", tostring(err))
							end)
						end)
					end,
					["messages"] = function(messagesPacket)
						if self.__status == Status.Disconnected then
							return
						end

						Log.debug("Received {} messages from Rojo server", #messagesPacket.messages)

						for index, message in messagesPacket.messages do
							local publicationCursor = messagesPacket.messageCursor - #messagesPacket.messages + index
							self:__applyPatch(message, publicationCursor)
						end
						self.__apiContext:setMessageCursor(messagesPacket.messageCursor)
						self:__releaseLocalProperties(messagesPacket.messageCursor)
					end,
				})
			end)
		end)
		:catch(function(err)
			if self.__status ~= Status.Disconnected then
				self:__stopInternal(err)
			end
		end)
end

function ServeSession:stop()
	self:__stopInternal()
end

function ServeSession:__applyGameAndPlaceId(serverInfo)
	if serverInfo.gameId ~= nil then
		game:SetUniverseId(serverInfo.gameId)
	end

	if serverInfo.placeId ~= nil then
		game:SetPlaceId(serverInfo.placeId)
	end
end

function ServeSession:__onActiveScriptChanged(activeScript)
	if not Settings:get("openScriptsExternally") then
		Log.trace("Not opening script {} because feature not enabled.", activeScript)

		return
	end

	if self.__status ~= Status.Connected then
		Log.trace("Not opening script {} because session is not connected.", activeScript)

		return
	end

	local scriptId = self.__instanceMap.fromInstances[activeScript]
	if scriptId == nil then
		Log.trace("Not opening script {} because it is not known by Rojo.", activeScript)

		return
	end

	Log.debug("Trying to open script {} externally...", activeScript)

	-- Force-close the script inside Studio... with a small delay in the middle
	-- to prevent Studio from crashing.
	task.defer(function()
		local existingParent = activeScript.Parent
		activeScript.Parent = nil

		for _ = 1, 3 do
			RunService.Heartbeat:Wait()
		end

		activeScript.Parent = existingParent
	end)

	-- Notify the Rojo server to open this script
	self.__apiContext:open(scriptId)
end

function ServeSession:__replaceInstances(idList)
	if #idList == 0 then
		return true, PatchSet.newEmpty()
	end
	-- It would be annoying if selection went away, so we try to preserve it.
	local selection = Selection:Get()
	local selectionMap = {}
	for i, instance in selection do
		selectionMap[instance] = i
	end

	-- TODO: Should we do this in multiple requests so we can more granularly mark failures?
	local modelSuccess, replacements = self.__apiContext
		:serialize(idList)
		:andThen(function(response)
			Log.debug("Deserializing results from serialize endpoint")
			local objects = SerializationService:DeserializeInstancesAsync(response.modelContents)
			if not objects[1] then
				return Promise.reject("Serialize endpoint did not deserialize into any Instances")
			end
			if #objects[1]:GetChildren() ~= #idList then
				return Promise.reject("Serialize endpoint did not return the correct number of Instances")
			end

			local instanceMap = {}
			for _, item in objects[1]:GetChildren() do
				instanceMap[item.Name] = item.Value
			end
			return instanceMap
		end)
		:await()

	local refSuccess, refPatch = self.__apiContext
		:refPatch(idList)
		:andThen(function(response)
			return response.patch
		end)
		:await()

	if not (modelSuccess and refSuccess) then
		return false
	end

	-- Roblox appends to GetChildren() on every reparent, so the order in which
	-- we re-parent replacements determines their final sibling order.
	-- We process ancestors before descendants (so each replacement's
	-- parent already exists when we re-parent it) and siblings in their original
	-- GetChildren() order. Because the loop below moves the old instance's
	-- children into the replacement *before* re-parenting the replacement, this
	-- rebuilds GetChildren() exactly as it was before the swap.
	local swaps = {}
	for id, replacement in replacements do
		local oldInstance = self.__instanceMap.fromIds[id]
		if not oldInstance then
			-- TODO: Why would this happen?
			Log.warn("Instance {} not found in InstanceMap during sync replacement", id)
			continue
		end

		table.insert(swaps, {
			id = id,
			replacement = replacement,
			oldInstance = oldInstance,
		})
	end

	-- Only the instances being swapped get re-appended, so untouched siblings
	-- stay put and replaced siblings land at the end. Record each affected
	-- parent's original order so we can restore it after the swaps, including
	-- when only some of a parent's children are replaced.
	local originalOrders = {}
	local replacementByOld = {}
	for _, swap in swaps do
		replacementByOld[swap.oldInstance] = swap.replacement

		local parent = swap.oldInstance.Parent
		if parent ~= nil and originalOrders[parent] == nil then
			originalOrders[parent] = parent:GetChildren()
		end
	end

	for _, swap in orderSwaps(swaps) do
		local id, replacement, oldInstance = swap.id, swap.replacement, swap.oldInstance

		self.__instanceMap:insert(id, replacement)
		self.__instanceMap:pauseInstance(replacement)
		Log.trace("Swapping Instance {} out via api/models/ endpoint", id)
		local oldParent = oldInstance.Parent
		for _, child in oldInstance:GetChildren() do
			self.__instanceMap:pauseInstance(child)
			-- Some children cannot be reparented, such as a TouchTransmitter
			local reparentSuccess, reparentError = attemptReparent(child, replacement)
			if reparentSuccess and self.__instanceMap.fromInstances[child] ~= nil then
				self.__instanceMap.knownParents[child] = replacement
				self.__instanceMap.observedParents[child] = replacement
			elseif not reparentSuccess then
				Log.warn(
					"Could not reparent child {} of instance {} during sync replacement: {}",
					child.Name,
					oldInstance.Name,
					reparentError
				)
			end
		end

		-- ChangeHistoryService doesn't like it if an Instance has been
		-- Destroyed. So, we have to accept the potential memory hit and
		-- just set the parent to `nil`.
		local deleteSuccess, deleteError = attemptReparent(oldInstance, nil)
		local replaceSuccess, replaceError = attemptReparent(replacement, oldParent)

		if not (deleteSuccess and replaceSuccess) then
			Log.warn(
				"Could not swap instances {} and {} during sync replacement: {}",
				oldInstance.Name,
				replacement.Name,
				(deleteError or "") .. "\n" .. (replaceError or "")
			)

			-- We need to revert the failed swap to avoid losing the old instance and children.
			for _, child in replacement:GetChildren() do
				if attemptReparent(child, oldInstance) and self.__instanceMap.fromInstances[child] ~= nil then
					self.__instanceMap.knownParents[child] = oldInstance
					self.__instanceMap.observedParents[child] = oldInstance
				end
			end
			attemptReparent(oldInstance, oldParent)
			self.__instanceMap:insert(id, oldInstance)

			-- Our replacement should never have existed in the first place, so we can just destroy it.
			replacement:Destroy()
			continue
		end

		self.__instanceMap.knownParents[replacement] = oldParent
		self.__instanceMap.observedParents[replacement] = oldParent

		if selectionMap[oldInstance] then
			-- This is a bit funky, but it saves the order of Selection
			-- which might matter for some use cases.
			selection[selectionMap[oldInstance]] = replacement
		end
	end

	-- Re-append the affected parents' children in their original order. This
	-- includes untouched siblings, which the swap loop never moved but that the
	-- replacements now sit after.
	for parent, originalChildren in originalOrders do
		-- If the parent itself was replaced, its children now live under the
		-- replacement, so restore order there instead.
		local targetParent = replacementByOld[parent] or parent
		for _, oldChild in originalChildren do
			local child = replacementByOld[oldChild] or oldChild
			if child.Parent == targetParent then
				-- Reassigning Parent appends the child, so walking the original
				-- order rebuilds GetChildren().
				attemptReparent(child, targetParent)
			end
		end
	end

	local patchApplySuccess, unappliedPatch = pcall(self.__reconciler.applyPatch, self.__reconciler, refPatch)
	if patchApplySuccess then
		Selection:Set(selection)
		return true, unappliedPatch
	else
		error(unappliedPatch)
	end
end

function ServeSession:__applyPatch(patch, publicationCursor)
	self:__keepUndoneDeletions(patch)
	patch = self:__protectLocalProperties(patch, publicationCursor)
	local patchTimestamp = DateTime.now():FormatLocalTime("LTS", "en-us")
	local historyRecording =
		ChangeHistoryService:TryBeginRecording(ServeSession.PATCH_RECORDING_PREFIX .. patchTimestamp)
	if not historyRecording then
		-- There can only be one recording at a time
		Log.debug("Failed to begin history recording for " .. patchTimestamp .. ". Another recording is in progress.")
	end

	Timer.start("precommitCallbacks", "wall")
	-- Precommit callbacks must be serial in order to obey the contract that
	-- they execute before commit
	for _, callback in self.__precommitCallbacks do
		local success, err = pcall(callback, patch, self.__instanceMap)
		if not success then
			Log.warn("Precommit hook errored: {}", err)
		end
	end
	Timer.stop()

	local patchApplySuccess, unappliedPatch = pcall(self.__reconciler.applyPatch, self.__reconciler, patch)
	if not patchApplySuccess then
		if historyRecording then
			ChangeHistoryService:FinishRecording(historyRecording, Enum.FinishRecordingOperation.Commit)
		end
		-- This might make a weird stack trace but the only way applyPatch can
		-- fail is if a bug occurs so it's probably fine.
		error(unappliedPatch)
	end

	if Settings:get("enableSyncFallback") and not PatchSet.isEmpty(unappliedPatch) then
		-- Some changes did not apply, let's try replacing them instead. Protected
		-- instances are never replaced, so their updates stay unapplied.
		local replaceableUpdates, protectedUpdates =
			ReconcilerServices.splitReplaceableUpdates(unappliedPatch.updated, self.__instanceMap)
		unappliedPatch.updated = replaceableUpdates

		local addedIdList = PatchSet.addedIdList(unappliedPatch)
		local updatedIdList = PatchSet.updatedIdList(unappliedPatch)

		Log.debug("ServeSession:__replaceInstances(unappliedPatch.added)")
		local addSuccess, unappliedAddedRefs = Timer.measure(
			"ServeSession:__replaceInstances(unappliedPatch.added)",
			"wall",
			self.__replaceInstances,
			self,
			addedIdList
		)

		Log.debug("ServeSession:__replaceInstances(unappliedPatch.updated)")
		local updateSuccess, unappliedUpdateRefs = Timer.measure(
			"ServeSession:__replaceInstances(unappliedPatch.updated)",
			"wall",
			self.__replaceInstances,
			self,
			updatedIdList
		)

		-- Update the unapplied patch to reflect which Instances were replaced successfully
		if addSuccess then
			table.clear(unappliedPatch.added)
			PatchSet.assign(unappliedPatch, unappliedAddedRefs)
		end
		if updateSuccess then
			table.clear(unappliedPatch.updated)
			PatchSet.assign(unappliedPatch, unappliedUpdateRefs)
		end
		for _, update in protectedUpdates do
			table.insert(unappliedPatch.updated, update)
		end
	end

	if not PatchSet.isEmpty(unappliedPatch) then
		Log.debug(
			"Could not apply all changes requested by the Rojo+ server:\n{}",
			PatchSet.humanSummary(self.__instanceMap, unappliedPatch)
		)
	end

	Timer.start("postcommitCallbacks")
	-- Postcommit callbacks can be called with spawn since regardless of firing order, they are
	-- guaranteed to be called after the commit
	for _, callback in self.__postcommitCallbacks do
		task.spawn(function()
			local success, err = pcall(callback, patch, self.__instanceMap, unappliedPatch)
			if not success then
				Log.warn("Postcommit hook errored: {}", err)
			end
		end)
	end
	Timer.stop()

	if historyRecording then
		ChangeHistoryService:FinishRecording(historyRecording, Enum.FinishRecordingOperation.Commit)
	end
end

function ServeSession:__initialSync(serverInfo)
	return self.__apiContext:read({ serverInfo.rootInstanceId }):andThen(function(readResponseBody)
		-- Tell the API Context that we're up-to-date with the version of
		-- the tree defined in this response.
		self.__apiContext:setMessageCursor(readResponseBody.messageCursor)

		-- For any instances that line up with the Rojo server's view, start
		-- tracking them in the reconciler.
		Log.trace("Matching existing Roblox instances to Rojo IDs")
		self:setLoadingText("Hydrating instance map...")
		-- Both walks cover the whole place, which on a large one outlasts
		-- Studio's script timeout unless they yield along the way.
		local step = self:__syncStep(yieldBudget())
		self.__reconciler:hydrate(readResponseBody.instances, serverInfo.rootInstanceId, game, step)

		-- Calculate the initial patch to apply to the DataModel to catch us
		-- up to what Rojo thinks the place should look like.
		Log.trace("Computing changes that plugin needs to make to catch up to server...")
		self:setLoadingText("Finding differences between server and Studio...")
		local success, catchUpPatch =
			self.__reconciler:diff(readResponseBody.instances, serverInfo.rootInstanceId, step)

		self:__releaseDeferredChanges()

		if not success then
			Log.error("Could not compute a diff to catch up to the Rojo server: {:#?}", catchUpPatch)
		end

		for _, update in catchUpPatch.updated do
			if update.id == self.__instanceMap.fromInstances[game] and update.changedClassName ~= nil then
				-- Non-place projects will try to update the classname of game from DataModel to
				-- something like Folder, ModuleScript, etc. This would fail, so we exit with a clear
				-- message instead of crashing.
				return Promise.reject(
					"Cannot sync a model as a place."
						.. "\nEnsure Rojo+ is serving a project file that has a DataModel at the root of its tree and try again."
						.. "\nSee project file docs: https://rojo.space/docs/v7/project-format/"
				)
			end
		end

		Log.trace("Computed hydration patch: {:#?}", debugPatch(catchUpPatch))

		local userDecision = "Accept"
		if self.__userConfirmCallback ~= nil then
			userDecision = self.__userConfirmCallback(self.__instanceMap, catchUpPatch, serverInfo)
		end

		if userDecision == "Abort" then
			return Promise.reject("Aborted Rojo+ sync operation")
		elseif userDecision == "Reject" then
			if not self.__twoWaySync then
				return Promise.reject("Cannot reject sync operation without two-way sync enabled")
			end
			-- The user wants their studio DOM to write back to their Rojo DOM
			-- so we will reverse the patch and send it back

			local inversePatch = PatchSet.newEmpty()

			-- Send back the current properties
			for _, change in catchUpPatch.updated do
				local instance = self.__instanceMap.fromIds[change.id]
				if not instance then
					continue
				end

				local properties = table.clone(change.changedProperties)
				if change.changedName ~= nil then
					properties.Name = true
				end
				if change.changedParent ~= nil then
					if instance.Parent == nil then
						table.insert(inversePatch.removed, change.id)
						continue
					end
					if self.__instanceMap.fromInstances[instance.Parent] == nil then
						return Promise.reject("Cannot reject a parent change to an unmanaged Studio parent")
					end
					properties.Parent = true
				end
				local update = encodePatchUpdate(instance, change.id, properties, self.__instanceMap)
				if update ~= nil then
					table.insert(inversePatch.updated, update)
				end
			end
			-- Add the removed instances back to Rojo
			-- selene:allow(empty_if, unused_variable, empty_loop)
			for _, instance in catchUpPatch.removed do
				-- TODO: Generate ID for our instance and add it to inversePatch.added
			end
			-- Remove the additions we've rejected
			for id, _change in catchUpPatch.added do
				table.insert(inversePatch.removed, id)
			end

			-- Rejecting is an explicit choice to keep Studio's version, so Studio
			-- wins even if the disk changes again before this write lands.
			return self.__apiContext:write(inversePatch, nil, "Studio")
		elseif userDecision == "Accept" then
			self:__applyPatch(catchUpPatch)
			return Promise.resolve()
		else
			return Promise.reject("Invalid user decision: " .. userDecision)
		end
	end)
end

function ServeSession:__stopInternal(err)
	if self.__status == Status.Disconnected then
		return
	end
	self.__status = Status.Disconnected
	self.__localPropertyEdits = nil
	self.__apiContext:disconnect()
	self.__instanceMap:stop()
	self.__changeBatcher:stop()

	for _, connection in ipairs(self.__connections) do
		connection:Disconnect()
	end
	self.__connections = {}

	-- Callbacks can reenter stop or throw. Notify only after cleanup, and keep a
	-- callback failure from interrupting the write queue's failure handling.
	local notified, problem = pcall(self.__setStatus, self, Status.Disconnected, err)
	if not notified then
		Log.warn("The disconnected status callback failed with {}", tostring(problem))
	end
end

function ServeSession:__setStatus(status, detail)
	self.__status = status

	if self.__statusChangedCallback ~= nil then
		self.__statusChangedCallback(status, detail)
	end
end

return ServeSession
