local StudioService = game:GetService("StudioService")
local RunService = game:GetService("RunService")
local ChangeHistoryService = game:GetService("ChangeHistoryService")
local SerializationService = game:GetService("SerializationService")
local Selection = game:GetService("Selection")
local ScriptEditorService = game:GetService("ScriptEditorService")
local HttpService = game:GetService("HttpService")

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
local SourceHistory = require(script.Parent.SourceHistory)
local orderSwaps = require(script.Parent.orderSwaps)
local yieldBudget = require(script.Parent.yieldBudget)

local STRUCTURAL_FIELDS = { Name = "changedName", Parent = "changedParent" }
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

local function attemptReparent(instanceMap, instance, parent)
	if instanceMap.fromInstances[instance] ~= nil then
		return instanceMap:applyStructuralProperty(instance, "Parent", parent)
	end
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

--[[
	Records the tracked instance each update and removal was encoded against. Class
	replacement tracks a new instance under the same id and removal untracks it, so
	comparing at dispatch shows which queued intent outlived its target. Additions are
	not captured, because a replacement carries their parent's children along with it.
]]
function ServeSession.captureOperationTargets(patch, fromIds)
	assert(t.table(patch))
	assert(t.table(fromIds))

	local targets = { Removed = {}, Updated = {} }
	for index, update in patch.updated do
		targets.Updated[index] = fromIds[update.id]
	end
	for index, id in patch.removed do
		targets.Removed[index] = fromIds[id]
	end
	return targets
end

--[[
	Splits an encoded write into what may still be sent and what an authoritative
	replacement or removal superseded after capture. Operations without a captured
	target are kept, so the server still decides about ids this session never tracked.
	The original patch is returned unchanged when nothing was superseded.
]]
function ServeSession.partitionSupersededOperations(patch, targets, fromIds)
	assert(t.table(patch))
	assert(t.table(targets))
	assert(t.table(fromIds))

	local superseded = PatchSet.newEmpty()
	local remaining = table.clone(patch)
	remaining.updated = {}
	remaining.removed = {}
	for index, update in patch.updated do
		local target = targets.Updated[index]
		local stale = target ~= nil and fromIds[update.id] ~= target
		table.insert(if stale then superseded.updated else remaining.updated, update)
	end
	for index, id in patch.removed do
		local target = targets.Removed[index]
		local stale = target ~= nil and fromIds[id] ~= target
		table.insert(if stale then superseded.removed else remaining.removed, id)
	end
	if PatchSet.isEmpty(superseded) then
		return patch, superseded
	end
	return remaining, superseded
end

local function supersededResult(superseded)
	return {
		accepted = { removed_instances = {}, added_instances = {}, updated_instances = {} },
		conflicts = { removed_instances = {}, added_instances = {}, updated_instances = {} },
		errors = {},
		superseded = superseded,
	}
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
	local function onInstanceChanged(instance, propertyName, observation)
		self:__onInstanceChanged(instance, propertyName, observation)
	end

	local function onChildAdded(child)
		self:__onChildAdded(child)
	end

	-- Each batch keeps the cursor it started from, taken before preparing it
	-- yields, since an edit made meanwhile starts the next batch.
	local function onBatchTaken(changes)
		local cursor = self.__pendingWriteCursor
		self.__pendingWriteCursor = nil
		local dependencies = self.__pendingWriteDependencies or {}
		self.__pendingWriteDependencies = nil
		-- Install before encoding can yield. Edits during preparation depend on this
		-- placeholder, not on whichever HTTP response happens to arrive next.
		local ticket = self:__newWriteTicket(dependencies)
		return { cursor = cursor, properties = self:__takeLocalPropertyEdits(changes), ticket = ticket }
	end

	local function onBatchPrepared(patch, context)
		-- Encoding just finished. Replaying omitted properties below runs precommit
		-- hooks that may yield while a publication replaces or removes a target.
		if patch ~= nil then
			context.targets = ServeSession.captureOperationTargets(patch, self.__instanceMap.fromIds)
		end
		if patch == nil or PatchSet.isEmpty(patch) or self.__status == Status.Disconnected then
			self:__resolveWriteTicket(context.ticket)
			self:__completeWriteTicket(context.ticket)
		end
		self:__finishOmittedLocalProperties(patch, context.properties)
	end

	local function onChangesFlushed(patch, context)
		return self:__writePatch(patch, context.cursor, context.properties, context.ticket, context.targets)
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
		__sourceRevisions = {},
		__sourceDrafts = {},
		__sourcePublicationCursors = setmetatable({}, { __mode = "k" }),
		__structuralPublicationCursors = setmetatable({}, { __mode = "k" }),
		__sourceBaselines = setmetatable({}, { __mode = "k" }),
		__sourcePendingCounts = setmetatable({}, { __mode = "k" }),
		__writeTickets = {},
		__remoteSources = {},
		__sourceApplyBatches = setmetatable({}, { __mode = "k" }),
		__sourceHistory = SourceHistory.new({
			Clock = os.clock,
			Horizon = 10,
			MaxBytes = 8 * 1024 * 1024,
			MaxEntries = 512,
		}),
		__precommitCallbacks = {},
		__postcommitCallbacks = {},
		__updateLoadingText = function() end,
	}

	setmetatable(self, ServeSession)
	instanceMap.onSourceScheduled = function(instance, operation)
		self:__onSourceScheduled(instance, operation)
	end
	instanceMap.onSourceOutcome = function(instance, operation)
		self:__onSourceOutcome(instance, operation)
	end
	instanceMap.onBeforeStructuralAssignment = function(instance, propertyName, value)
		self:__beforeStructuralAssignment(instance, propertyName, value)
	end

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
function ServeSession:__onSourceScheduled(instance, operation)
	if self.__status == Status.Disconnected then
		return
	end
	local id = self.__instanceMap.fromInstances[instance]
	if id == nil then
		return
	end
	if self.__remoteSources[operation] ~= nil then
		return
	end
	self.__sourcePendingCounts[instance] = (self.__sourcePendingCounts[instance] or 0) + 1
	local properties = self.__localPropertyEdits and self.__localPropertyEdits[id]
	local pending = {
		Id = id,
		Instance = instance,
		BeforeDraft = self.__sourceDrafts[id],
		BeforeEntry = properties and properties.Source,
		BeforeRevision = self:__sourceRevision(id, instance),
		Operation = operation,
	}
	pending.Promise = Promise.new(function(resolve)
		pending.Resolve = resolve
	end)
	self.__remoteSources[operation] = pending
	local batch = self.__sourceApplyBatches[coroutine.running()]
	if batch ~= nil then
		pending.PublicationCursor = batch.Cursors and batch.Cursors[id]
		pending.InBatch = true
		table.insert(batch, pending)
	end
end

function ServeSession:__resolveSourceBaseline(token, cursor)
	if token.Resolve == nil then
		return
	end
	local previous = token.Previous
	local resolved = if cursor ~= nil
		then Promise.resolve(cursor)
		elseif previous ~= nil then previous.Promise or Promise.resolve(previous.Cursor)
		else Promise.resolve()
	resolved:andThen(function(value)
		if token.Resolve == nil then
			return
		end
		token.Cursor = value
		if cursor == nil then
			token.Revision = previous and previous.Revision
		end
		local resolve = token.Resolve
		token.Previous, token.Resolve, token.Promise = nil, nil, nil
		resolve(value)
	end)
end

function ServeSession:__onSourceOutcome(instance, operation)
	local pending = self.__remoteSources[operation]
	if pending == nil then
		return
	end
	self.__remoteSources[operation] = nil
	local remaining = self.__sourcePendingCounts[instance] - 1
	self.__sourcePendingCounts[instance] = if remaining > 0 then remaining else nil
	local id = pending.Id
	if self.__status ~= Status.Disconnected and self.__instanceMap.fromIds[id] == instance then
		local properties = self.__localPropertyEdits and self.__localPropertyEdits[id]
		local observed = pending.Observation
		local visible, current = pcall(self.__readEditorSource, self, instance)
		local committedObservation = operation.Status == "committed"
			and observed ~= nil
			and observed.Revision == operation.CommittedRevision
		local confirmedObservation = committedObservation and visible and current == operation.Value
		if pending.BaselineToken ~= nil then
			self:__resolveSourceBaseline(
				pending.BaselineToken,
				if committedObservation then pending.PublicationCursor else nil
			)
		elseif
			operation.Status == "committed"
			and (operation.NoChange or observed == nil)
			and pending.PublicationCursor ~= nil
			and self:__sourceRevision(id, instance) == pending.BeforeRevision
			and visible
			and current == operation.Value
		then
			self.__sourceBaselines[instance] = { Cursor = pending.PublicationCursor, Revision = pending.BeforeRevision }
		end
		if confirmedObservation then
			if properties and properties.Source == pending.ObservedEntry then
				properties.Source = nil
				if next(properties) == nil then
					self.__localPropertyEdits[id] = nil
				end
			end
			if self.__sourceDrafts[id] == pending.ObservedDraft then
				self.__sourceDrafts[id] = nil
			end
		elseif operation.Status == "committed" and self:__sourceRevision(id, instance) == pending.BeforeRevision then
			local draft = pending.BeforeDraft
			local secured = draft == nil
				or draft.Source == operation.Value
				or draft.CheckpointSource == operation.Expected
			local read, value = pcall(self.__readEditorSource, self, instance)
			if secured and read and value == operation.Value and self.__sourceDrafts[id] == draft then
				self.__sourceDrafts[id] = nil
			end
		elseif pending.ObservedEntry ~= nil then
			-- A proposal that the engine retried or rejected never owns the observed edit.
			self:__queueObservedSource(instance, pending.ObservedEntry)
		end
	end
	if pending.BaselineToken ~= nil then
		self:__resolveSourceBaseline(pending.BaselineToken)
	end
	local resolve = pending.Resolve
	pending.Resolve = nil
	resolve(operation)
end

function ServeSession:__awaitSourceOperations(operations)
	if #operations == 0 then
		return nil
	end
	local promises = {}
	for _, pending in ipairs(operations) do
		table.insert(
			promises,
			pending.Promise:andThen(function(operation)
				if operation.Status == "committed" or operation.Status == "superseded" then
					return
				end
				if self.__status == Status.Disconnected then
					return Promise.reject("Source application was cancelled with its session.")
				end
				local instance = self.__instanceMap.fromIds[pending.Id]
				if
					instance == pending.Instance
					and self.__sourceHistory:classify(
							pending.Id,
							operation.ServerOperation,
							instance,
							self:__sourceRevision(pending.Id, instance)
						)
						== "Echo"
				then
					return
				end
				return Promise.reject(
					if operation.Status == "failed"
						then "The editor failed to apply authoritative Source. Your draft was retained."
						else "The editor changed while authoritative Source was being applied. Your newer draft was retained; reconnect to resolve the conflict."
				)
			end)
		)
	end
	return Promise.all(promises)
end

function ServeSession:__queueObservedSource(instance, entry)
	local id = self.__instanceMap.fromInstances[instance]
	local properties = id and self.__localPropertyEdits and self.__localPropertyEdits[id]
	if properties == nil or properties.Source ~= entry or entry.queued then
		return
	end
	if self.__deferredChanges ~= nil then
		if not entry.deferred then
			entry.deferred = true
			table.insert(self.__deferredChanges, { instance = instance, propertyName = "Source", sourceEntry = entry })
		end
		return
	end
	if self.__changeBatcher:isEmpty() then
		self.__pendingWriteCursor = entry.baselineCursor
	elseif self.__pendingWriteCursor ~= nil then
		self.__pendingWriteCursor = math.min(self.__pendingWriteCursor, entry.baselineCursor)
	end
	entry.queued = true
	self:__captureWriteDependency(entry.predecessor)
	self.__changeBatcher:add(instance, "Source")
end

function ServeSession:__onInstanceChanged(instance, propertyName, observation)
	if not self.__twoWaySync then
		return
	end
	if propertyName == "Source" and observation ~= nil then
		local _, generation = self.__instanceMap:getSourceRevision(instance)
		if generation ~= observation.Generation then
			return
		end
		if observation.Error ~= nil then
			self:__stopInternal("The current editor Source could not be read safely.")
			return
		end
		self:__rememberLocalProperty(instance, "Source", observation)
		local id = self.__instanceMap.fromInstances[instance]
		local entry = self.__localPropertyEdits[id].Source
		local remote = observation.Operation and self.__remoteSources[observation.Operation]
		if remote ~= nil then
			remote.Observation = observation
			remote.ObservedEntry = entry
			remote.ObservedDraft = self.__sourceDrafts[id]
			-- Notification is provisional. A genuine edit before the engine returns
			-- captures this token and waits for its exact committed observation.
			if remote.PublicationCursor ~= nil then
				if remote.BaselineToken ~= nil then
					self:__resolveSourceBaseline(remote.BaselineToken)
				end
				local token = { Previous = self.__sourceBaselines[instance], Revision = entry.revision }
				token.Promise = Promise.new(function(resolve)
					token.Resolve = resolve
				end)
				remote.BaselineToken = token
				self.__sourceBaselines[instance] = token
			end
		else
			self:__queueObservedSource(instance, entry)
		end
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
		elseif change.sourceEntry ~= nil then
			change.sourceEntry.deferred = nil
			self:__queueObservedSource(change.instance, change.sourceEntry)
		else
			self:__queueChange(change.instance, change.propertyName)
		end
	end
end

-- Retain the revision preceding the first local change, even if a websocket
-- update arrives before this batch is sent.
function ServeSession:__markBatchStart()
	self:__captureWriteDependency(self.__writeHead)
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

function ServeSession:__readEditorSource(instance)
	return ScriptEditorService:GetEditorSource(instance)
end

function ServeSession:__updateEditorSource(instance, callback)
	return ScriptEditorService:UpdateSourceAsync(instance, callback)
end

function ServeSession:__hasOpenScriptDocument(instance)
	return ScriptEditorService:FindScriptDocument(instance) ~= nil
end

function ServeSession:__rememberSourcePublication(patch, cursor)
	for _, update in patch.updated do
		local instance = self.__instanceMap.fromIds[update.id]
		if instance ~= nil and update.changedProperties and update.changedProperties.Source ~= nil then
			local previous = self.__sourcePublicationCursors[instance]
			if previous == nil or previous < cursor then
				self.__sourcePublicationCursors[instance] = cursor
			end
		end
	end
end

function ServeSession:__sourceRevision(id, instance)
	local revision = self.__sourceRevisions[id]
	return if revision and revision.Instance == instance then revision.Revision else 0
end

function ServeSession:__filterSourceEchoes(patch)
	if patch.sourceOperations == nil and patch.sourceReplays == nil then
		return patch
	end
	local filtered = table.clone(patch)
	filtered.updated = {}
	for _, update in patch.updated do
		local instance = self.__instanceMap.fromIds[update.id]
		local properties = update.changedProperties
		local operation = patch.sourceOperations and patch.sourceOperations[update.id]
		local replay = patch.sourceReplays and patch.sourceReplays[update.id]
		local latest = instance and self.__sourcePublicationCursors[instance]
		local supersededReplay = replay ~= nil
			and (replay.Instance ~= instance or (replay.Cursor ~= nil and latest ~= nil and replay.Cursor < latest))
		if
			properties ~= nil
			and properties.Source ~= nil
			and (
				supersededReplay
				or (
					instance ~= nil
					and self.__sourceHistory:classify(
							update.id,
							operation,
							instance,
							self:__sourceRevision(update.id, instance)
						)
						== "Echo"
				)
			)
		then
			local kept = table.clone(update)
			kept.changedProperties = table.clone(properties)
			kept.changedProperties.Source = nil
			kept.sourceSuppressed = true
			table.insert(filtered.updated, kept)
		else
			table.insert(filtered.updated, update)
		end
	end
	return filtered
end

local function localPropertyName(instance, propertyName)
	-- Structure has literal patch fields, not writable scalar Ref descriptors.
	if STRUCTURAL_FIELDS[propertyName] ~= nil then
		return propertyName
	end
	local descriptor = getWritablePropertyDescriptor(instance.ClassName, propertyName)
	return if descriptor ~= nil and descriptor.dataType ~= "Ref" then descriptor.name else nil
end

function ServeSession:__rememberLocalProperty(instance, propertyName, observation)
	local instanceMap = self.__instanceMap
	local id = instanceMap and instanceMap.fromInstances[instance]
	if id == nil or instance:IsA("DataModel") then
		return
	end
	-- Buffer local generations until the server decides authority, including any
	-- rejected-write restoration. Buffering does not prove a publication is our own.
	local name = localPropertyName(instance, propertyName)
	if name == nil then
		return
	end
	self.__localPropertyEdits = self.__localPropertyEdits or {}
	local properties = self.__localPropertyEdits[id] or {}
	self.__localPropertyEdits[id] = properties
	-- Each entry is a generation token. A previous request cannot release a later edit.
	local entry = { instance = instance, predecessor = self.__writeHead }
	local previous = properties[name]
	if STRUCTURAL_FIELDS[name] ~= nil and previous ~= nil and previous.instance == instance then
		-- Keep the last received server value, never the older generation's coverage.
		-- A refusal need not publish the same authoritative field a second time.
		entry.serverValue = previous.serverValue
		entry.serverCursor = previous.serverCursor
	end
	if name == "Source" then
		local revision = self:__sourceRevision(id, instance) + 1
		self.__sourceRevisions[id] = { Instance = instance, Revision = revision }
		entry.revision = revision
		entry.baselineCursor = self.__apiContext:getMessageCursor()
		entry.sourceBaseline = self.__sourceBaselines[instance]
		self.__sourceDrafts[id] = {
			Instance = instance,
			Revision = revision,
			Source = if observation then observation.Value else self:__readEditorSource(instance),
		}
	end
	properties[name] = entry
end

function ServeSession:__takeLocalPropertyEdits(changes)
	local taken = {}
	for instance, changed in changes do
		local id = self.__instanceMap.fromInstances[instance]
		local properties = self.__localPropertyEdits[id]
		if properties ~= nil then
			local entries = {}
			for propertyName in changed do
				local name = localPropertyName(instance, propertyName)
				if name ~= nil then
					entries[name] = properties[name]
				end
			end
			taken[id] = entries
		end
	end
	return taken
end

function ServeSession:__finishOmittedLocalProperties(patch, taken)
	local emitted = {}
	local removed = {}
	if patch ~= nil then
		for _, id in patch.removed do
			removed[id] = true
		end
		for _, update in patch.updated do
			local properties = emitted[update.id] or {}
			emitted[update.id] = properties
			for propertyName in update.changedProperties or {} do
				properties[propertyName] = true
			end
			for propertyName, field in STRUCTURAL_FIELDS do
				if update[field] ~= nil then
					properties[propertyName] = true
				end
			end
		end
	end
	local omitted = {}
	for id, entries in taken do
		local properties = emitted[id]
		for propertyName, entry in entries do
			if removed[id] and STRUCTURAL_FIELDS[propertyName] ~= nil then
				-- Nil Parent is encoded as removal. Its deletion lifecycle now owns
				-- authority; replaying a buffered parent here would undo the deletion.
				local current = self.__localPropertyEdits and self.__localPropertyEdits[id]
				if current and current[propertyName] == entry then
					current[propertyName] = nil
					if next(current) == nil then
						self.__localPropertyEdits[id] = nil
					end
				end
			elseif properties == nil or properties[propertyName] == nil then
				omitted[id] = omitted[id] or {}
				omitted[id][propertyName] = entry
			elseif STRUCTURAL_FIELDS[propertyName] ~= nil then
				entry.encoded = true
			end
		end
	end
	self:__finishLocalProperties(omitted, nil)
end

function ServeSession:__rememberStructuralPublication(patch, cursor)
	self.__structuralPublicationCursors = self.__structuralPublicationCursors or setmetatable({}, { __mode = "k" })
	for _, update in patch.updated do
		local instance = self.__instanceMap.fromIds[update.id]
		if instance ~= nil then
			local cursors = self.__structuralPublicationCursors[instance] or {}
			for propertyName, field in STRUCTURAL_FIELDS do
				if update[field] ~= nil and (cursors[propertyName] == nil or cursors[propertyName] < cursor) then
					cursors[propertyName] = cursor
				end
			end
			if next(cursors) ~= nil then
				self.__structuralPublicationCursors[instance] = cursors
			end
		end
	end
end

function ServeSession:__filterStructuralReplays(patch)
	if patch.structuralReplays == nil then
		return patch
	end
	local filtered = table.clone(patch)
	filtered.updated = {}
	for _, update in patch.updated do
		local kept = table.clone(update)
		local replays = patch.structuralReplays[update.id] or {}
		local instance = self.__instanceMap.fromIds[update.id]
		local cursors = instance
			and self.__structuralPublicationCursors
			and self.__structuralPublicationCursors[instance]
		for propertyName, replay in replays do
			local latest = cursors and cursors[propertyName]
			-- These are internal replay receipts, not own-publication proof. Check at
			-- execution too, since Source recovery and hooks can delay the replay.
			if replay.Instance ~= instance or (latest ~= nil and replay.Cursor ~= nil and latest > replay.Cursor) then
				kept[STRUCTURAL_FIELDS[propertyName]] = nil
			end
		end
		table.insert(filtered.updated, kept)
	end
	return filtered
end

function ServeSession:__pruneStructuralGuards()
	for id, properties in self.__localPropertyEdits or {} do
		for propertyName in STRUCTURAL_FIELDS do
			local entry = properties[propertyName]
			if entry ~= nil and self.__instanceMap.fromIds[id] ~= entry.instance then
				properties[propertyName] = nil
			end
		end
		if next(properties) == nil then
			self.__localPropertyEdits[id] = nil
		end
	end
end

function ServeSession:__prepareStructuralParents(patch)
	local hasPendingParent = false
	for _, properties in self.__localPropertyEdits do
		if properties.Parent ~= nil then
			hasPendingParent = true
			break
		end
	end
	if not hasPendingParent then
		return patch
	end
	local instanceMap = self.__instanceMap
	local planned = {}
	local incomingParents = {}
	local retired = {}
	local updates = {}
	for _, update in patch.updated do
		local instance = instanceMap.fromIds[update.id]
		if instance ~= nil and update.changedParent ~= nil then
			incomingParents[instance] = instanceMap.fromIds[update.changedParent]
		end
	end
	local function wouldCycle(target, parent)
		local visited = {}
		while parent ~= nil do
			if parent == target or visited[parent] then
				return true
			end
			visited[parent] = true
			parent = planned[parent] or parent.Parent
		end
		return false
	end
	for _, update in patch.updated do
		local target = instanceMap.fromIds[update.id]
		local parent = instanceMap.fromIds[update.changedParent]
		local properties = self.__localPropertyEdits[update.id]
		local pending = properties and properties.Parent
		if target ~= nil and parent ~= nil and (pending == nil or retired[pending]) then
			while wouldCycle(target, parent) do
				local ancestor = parent
				local correction
				while ancestor ~= nil and ancestor ~= target do
					local id = instanceMap.fromInstances[ancestor]
					local entries = self.__localPropertyEdits[id]
					local entry = entries and entries.Parent
					local confirmed = incomingParents[ancestor]
						or (
							if entry and entry.serverValue ~= nil
								then instanceMap.fromIds[entry.serverValue]
								else instanceMap.knownParents[ancestor]
						)
					if
						entry ~= nil
						and entry.encoded
						and not retired[entry]
						and confirmed ~= nil
						and instanceMap.fromInstances[confirmed] ~= nil
						and confirmed ~= (planned[ancestor] or ancestor.Parent)
						and not wouldCycle(ancestor, confirmed)
					then
						-- The encoded request still carries the original intent for server
						-- validation. Restore confirmed ancestry before applying its reverse
						-- edge, never silently change a batch that has yet to encode.
						correction =
							{ id = id, changedParent = instanceMap.fromInstances[confirmed], changedProperties = {} }
						planned[ancestor] = confirmed
						retired[entry] = id
						break
					end
					ancestor = planned[ancestor] or ancestor.Parent
				end
				if correction == nil then
					error("The structural publication cannot be applied without a cycle. Local intent was retained.", 0)
				end
				table.insert(updates, correction)
			end
			planned[target] = parent
		end
		table.insert(updates, update)
	end
	if next(retired) == nil then
		return patch
	end
	for entry, id in retired do
		local properties = self.__localPropertyEdits[id]
		if properties and properties.Parent == entry then
			properties.Parent = nil
			if next(properties) == nil then
				self.__localPropertyEdits[id] = nil
			end
		end
	end
	local prepared = table.clone(patch)
	prepared.updated = updates
	return prepared
end

function ServeSession:__captureStructuralChanges(patch)
	local instanceMap = self.__instanceMap
	local instances = {}
	for id, properties in self.__localPropertyEdits or {} do
		if properties.Name ~= nil or properties.Parent ~= nil then
			local instance = instanceMap.fromIds[id]
			if instance ~= nil then
				instances[instance] = true
			end
		end
	end
	if patch ~= nil then
		for _, update in patch.updated do
			if update.changedName ~= nil or update.changedParent ~= nil or update.changedClassName ~= nil then
				local instance = instanceMap.fromIds[update.id]
				if instance ~= nil then
					instances[instance] = true
				end
			end
			if update.changedParent ~= nil then
				local ancestor = instanceMap.fromIds[update.changedParent]
				local visited = {}
				while ancestor ~= nil and not visited[ancestor] do
					visited[ancestor] = true
					instances[ancestor] = true
					ancestor = ancestor.Parent
				end
			end
		end
		for _, id in patch.removed do
			local instance = if typeof(id) == "Instance" then id else instanceMap.fromIds[id]
			if instance ~= nil then
				instances[instance] = true
			end
		end
		for id in patch.added do
			local instance = instanceMap.fromIds[id]
			if instance ~= nil then
				instances[instance] = true
			end
		end
	end
	for instance in instances do
		instanceMap:captureStructuralChanges(instance)
	end
end

function ServeSession:__beforeStructuralAssignment(instance, propertyName, value)
	if not self.__twoWaySync then
		return
	end
	local instanceMap = self.__instanceMap
	local observed = if propertyName == "Parent" then instanceMap.observedParents else instanceMap.observedNames
	if observed[instance] == instance[propertyName] then
		return
	end
	local id = instanceMap.fromInstances[instance]
	local signals = instanceMap.instancesToSignal[instance]
	instanceMap:captureStructuralChanges(instance)
	if
		self.__status == Status.Disconnected
		or instanceMap.fromInstances[instance] ~= id
		or instanceMap.fromIds[id] ~= instance
		or instanceMap.instancesToSignal[instance] ~= signals
	then
		error("The structural assignment outlived its tracked session. Local values were retained.", 0)
	end
	-- The normal filter captures before choosing guards. If a reentrant/yielding engine
	-- boundary changes it afterwards, stop rather than erase an unencoded generation.
	if instance[propertyName] ~= value then
		error("A local structural edit arrived during reconciliation. Its value was retained.", 0)
	end
end

function ServeSession:__protectLocalProperties(patch, publicationCursor)
	self:__captureStructuralChanges(patch)
	if self.__status == Status.Disconnected then
		error(ServeSession.SYNC_CANCELLED, 0)
	end
	self:__pruneStructuralGuards()
	patch = self:__filterStructuralReplays(self:__filterSourceEchoes(patch))
	local edits = self.__localPropertyEdits
	if edits == nil or next(edits) == nil then
		return patch
	end
	patch = self:__prepareStructuralParents(patch)
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
			local removingAncestor = false
			while instance ~= nil do
				local ancestorId = instanceMap.fromInstances[instance]
				invalidated = invalidated or structural[ancestorId] == true or structural[instance] == true
				removingAncestor = removingAncestor or removed[ancestorId] == true or removed[instance] == true
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
				-- Surviving moves/renames do not settle another pending generation. A
				-- removal must still permit escape moves before destroying its subtree.
				local retained = {}
				local restored = {}
				for propertyName, entry in properties do
					if
						not discarded
						and (propertyName == "Source" or (STRUCTURAL_FIELDS[propertyName] and not removingAncestor))
					then
						retained[propertyName] = entry
					elseif STRUCTURAL_FIELDS[propertyName] == nil then
						restored[propertyName] = entry.serverValue
					end
				end
				edits[id] = if next(retained) ~= nil then retained else nil
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
		for propertyName, field in STRUCTURAL_FIELDS do
			local value = kept[field]
			local entry = properties[propertyName]
			if value ~= nil and entry ~= nil then
				local replays = patch.structuralReplays and patch.structuralReplays[update.id]
				local replay = replays and replays[propertyName]
				local cursor = if replay then replay.Cursor else publicationCursor
				if entry.serverCursor == nil or cursor == nil or cursor >= entry.serverCursor then
					entry.serverValue = value
					entry.serverCursor = cursor
				end
				kept[field] = nil
			end
		end
		for propertyName, value in kept.changedProperties do
			local descriptor = instance and getWritablePropertyDescriptor(instance.ClassName, propertyName)
			local entry = properties[if descriptor ~= nil then descriptor.name else propertyName]
			if entry ~= nil then
				entry.serverValue = value
				entry.serverCursor = publicationCursor
				entry.serverInstance = instance
				entry.serverOperation = patch.sourceOperations and patch.sourceOperations[update.id]
				if propertyName == "Source" then
					kept.sourceSuppressed = true
				end
				kept.changedProperties[propertyName] = nil
			end
		end
		if
			next(kept.changedProperties) ~= nil
			or kept.changedMetadata ~= nil
			or kept.changedName ~= nil
			or kept.changedParent ~= nil
			or kept.changedClassName ~= nil
		then
			table.insert(filtered.updated, kept)
		end
	end
	return filtered
end

function ServeSession:__releaseLocalProperties(cursor)
	self:__captureStructuralChanges()
	self:__pruneStructuralGuards()
	local edits = self.__localPropertyEdits
	if edits == nil then
		return nil
	end
	local patch = PatchSet.newEmpty()
	patch.sourceOperations = {}
	patch.sourceReplays = {}
	patch.structuralReplays = {}
	for id, properties in edits do
		local released = {}
		local update = { id = id, changedProperties = released }
		for propertyName, entry in properties do
			if entry.cursor ~= nil and entry.cursor <= cursor then
				properties[propertyName] = nil
				if not entry.preserveLocal or (entry.serverCursor ~= nil and entry.serverCursor > entry.cursor) then
					local field = STRUCTURAL_FIELDS[propertyName]
					if field ~= nil then
						update[field] = entry.serverValue
						patch.structuralReplays[id] = patch.structuralReplays[id] or {}
						patch.structuralReplays[id][propertyName] =
							{ Instance = entry.instance, Cursor = entry.serverCursor }
					else
						released[propertyName] = entry.serverValue
					end
					if propertyName == "Source" then
						patch.sourceOperations[id] = entry.serverOperation
						patch.sourceReplays[id] = { Instance = entry.serverInstance, Cursor = entry.serverCursor }
					end
				end
			end
		end
		if next(properties) == nil then
			edits[id] = nil
		end
		if
			(next(released) ~= nil or update.changedName ~= nil or update.changedParent ~= nil)
			and self.__instanceMap.fromIds[id] ~= nil
		then
			table.insert(patch.updated, update)
		end
	end
	if not PatchSet.isEmpty(patch) then
		return self:__applyPatch(patch)
	end
	return nil
end

function ServeSession:__finishLocalProperties(taken, result)
	if taken == nil or next(taken) == nil or self.__localPropertyEdits == nil then
		return nil
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
	return self:__releaseLocalProperties(cursor)
end

function ServeSession:__queueChange(instance, propertyName)
	self:__rememberLocalProperty(instance, propertyName)
	self:__markBatchStart()
	self.__changeBatcher:add(instance, propertyName)
end

function ServeSession.sourceWriteSucceeded(result, id, ancestry)
	assert(t.table(result) and t.string(id) and t.table(ancestry), "Source acknowledgement identity must be valid.")
	local ancestors = { [id] = true }
	for _, ancestor in ancestry do
		ancestors[ancestor] = true
	end
	for _, failure in result.errors do
		if failure.id == id and (failure.property == nil or failure.property == "Source") then
			return false
		end
		if
			ancestors[failure.id]
			and (failure.property == nil or failure.property == "Parent" or failure.property == "Name")
		then
			return false
		end
	end
	for _, removed in result.conflicts.removed_instances do
		if ancestors[removed] then
			return false
		end
	end
	for _, added in result.conflicts.added_instances do
		if ancestors[added.instance.snapshot_id] then
			return false
		end
	end
	for _, update in result.conflicts.updated_instances do
		if update.id == id and update.changed_properties and update.changed_properties.Source ~= nil then
			return false
		end
		if
			ancestors[update.id]
			and (
				update.changed_name ~= nil
				or update.changedParent ~= nil
				or update.changed_class_name ~= nil
				or update.changed_metadata ~= nil
			)
		then
			return false
		end
	end
	-- A successfully validated no-op has no accepted patch, but its Source was saved too.
	return true
end

function ServeSession:__acknowledgeSourceDrafts(sources, result)
	for id, source in sources do
		local draft = self.__sourceDrafts[id]
		if draft ~= nil and draft.Instance == source.Instance and draft.Revision == source.Revision then
			local ancestry = {}
			local parent = source.Instance.Parent
			while parent ~= nil do
				local ancestor = self.__instanceMap.fromInstances[parent]
				if ancestor ~= nil then
					table.insert(ancestry, ancestor)
				end
				parent = parent.Parent
			end
			if ServeSession.sourceWriteSucceeded(result, id, ancestry) then
				self.__sourceDrafts[id] = nil
			end
		end
	end
end

-- Histories contain only private token strings, never strong ticket ancestry. Keeping
-- at most the server's live capability window bounds both storage and intersections.
local function commonLineage(histories)
	local common = table.clone(histories[1] or {})
	for index = 2, #histories do
		local included = {}
		for _, token in histories[index] do
			included[token] = true
		end
		local kept = {}
		for _, token in common do
			if included[token] then
				table.insert(kept, token)
			end
		end
		common = kept
	end
	return common
end

function ServeSession:__newWriteTicket(dependencies)
	local ticket = { Parent = dependencies, PreviousWrite = self.__lastWrite or Promise.resolve() }
	ticket.Promise = Promise.new(function(resolve)
		ticket.Resolve = resolve
	end)
	-- Reserve serialization order at capture too. A direct caller during encoding
	-- must not make the preparing batch wait on its own dependent write.
	self.__lastWrite = Promise.new(function(resolve)
		ticket.Complete = resolve
	end)
	self.__writeTickets = self.__writeTickets or {}
	self.__writeTickets[ticket] = true
	local promises = {}
	for _, dependency in dependencies do
		table.insert(promises, if dependency == false then Promise.resolve({}) else dependency.Promise)
	end
	ticket.Dependency = Promise.all(promises):andThen(commonLineage)
	self.__writeHead = ticket
	return ticket
end

function ServeSession:__resolveWriteTicket(ticket, acknowledgement)
	if ticket.Resolving or ticket.Resolve == nil then
		return
	end
	ticket.Resolving = true
	ticket.Dependency:andThen(function(lineage)
		if ticket.Resolve == nil then
			return
		end
		local kept = table.clone(lineage)
		if acknowledgement ~= nil then
			table.insert(kept, acknowledgement)
			if #kept > 256 then
				table.remove(kept, 1)
			end
		end
		local resolve = ticket.Resolve
		ticket.Parent, ticket.Resolve, ticket.Dependency = nil, nil, nil
		if ticket.Complete == nil then
			self.__writeTickets[ticket] = nil
		end
		resolve(kept)
	end)
end

function ServeSession:__completeWriteTicket(ticket)
	local previous = ticket.PreviousWrite or Promise.resolve()
	ticket.PreviousWrite = nil
	previous:andThen(function()
		local complete = ticket.Complete
		if complete == nil then
			return
		end
		ticket.Complete = nil
		if ticket.Resolve == nil then
			self.__writeTickets[ticket] = nil
		end
		complete()
	end)
end

function ServeSession:__captureWriteDependency(dependency)
	local dependencies = self.__pendingWriteDependencies or {}
	self.__pendingWriteDependencies = dependencies
	local captured = dependency or false
	if not table.find(dependencies, captured) then
		table.insert(dependencies, captured)
	end
end

function ServeSession:__writePatch(patch, messageCursor, localProperties, ticket, targets)
	if ticket == nil then
		local dependencies = {}
		if localProperties ~= nil then
			for _, entries in localProperties do
				for _, entry in entries do
					local captured = entry.predecessor or false
					if not table.find(dependencies, captured) then
						table.insert(dependencies, captured)
					end
				end
			end
		end
		if #dependencies == 0 then
			table.insert(dependencies, self.__writeHead or false)
		end
		ticket = self:__newWriteTicket(dependencies)
	end
	messageCursor = messageCursor or self.__apiContext:getMessageCursor()
	-- Batches bring the targets captured when they were encoded. Direct callers write
	-- what they just built without yielding, so their current targets are the encoded ones.
	targets = targets
		or ServeSession.captureOperationTargets(patch, if self.__instanceMap then self.__instanceMap.fromIds else {})
	local dependency = ticket.Dependency or Promise.resolve({})
	-- Read when each batch is sent, so changing it applies to the next write
	-- without reconnecting. A pinned session authority wins over the setting.
	local authority = self.__twoWaySyncAuthority or Settings:get("twoWaySyncAuthority")
	local operationId = HttpService:GenerateGUID(false)
	local sources = {}
	local baselinePromises = {}
	local sourceBaselines = {}
	local structuralWrite = false
	for _, update in patch.updated do
		structuralWrite = structuralWrite or update.changedName ~= nil or update.changedParent ~= nil
		local value = update.changedProperties and update.changedProperties.Source
		local instance = self.__instanceMap.fromIds[update.id]
		if value and instance then
			local taken = localProperties and localProperties[update.id]
			local entry = taken and taken.Source
			local baseline = if entry then entry.sourceBaseline else self.__sourceBaselines[instance]
			if baseline ~= nil then
				table.insert(
					baselinePromises,
					(baseline.Promise or Promise.resolve(baseline.Cursor)):andThen(function(cursor)
						sourceBaselines[update.id] = cursor
					end)
				)
			end
			sources[update.id] = {
				Instance = instance,
				Revision = if entry and entry.revision
					then entry.revision
					else self:__sourceRevision(update.id, instance),
				Source = value.String,
			}
		end
	end

	-- One record per write, which moves from Pending to its outcome, so a write
	-- is never reported as both pending and finished.
	local record = {
		status = "Pending",
		patch = patch,
		authority = authority,
	}
	table.insert(self.__syncResults, record)

	local function finish(status, result, target)
		target = target or record
		if result and result.acknowledgement ~= nil then
			result = table.clone(result)
			result.acknowledgement = nil
		end
		target.status = status
		target.result = result
		if self.__syncResultCallback ~= nil then
			self.__syncResultCallback(status, result)
		end
	end

	-- The instances this write deletes are remembered until it's answered, so
	-- an announcement of the deletion can tell whether an undo came first.
	-- Each guard belongs to the instance this write encoded, so a target already
	-- replaced or removed gets none, and an id reused by a newer instance keeps its own.
	self.__pendingDeletions = self.__pendingDeletions or {}
	for index, id in patch.removed do
		local target = targets.Removed[index]
		if target ~= nil and self.__instanceMap ~= nil and self.__instanceMap.fromIds[id] == target then
			self.__pendingDeletions[id] = target
		end
	end
	local function forgetDeletions()
		for index, id in patch.removed do
			if self.__pendingDeletions[id] == targets.Removed[index] then
				self.__pendingDeletions[id] = nil
			end
		end
	end

	-- Writes reach the server in the order they were made, so a later edit to
	-- an instance never arrives before the write that created it. A write
	-- still waiting when the session stops is never sent.
	local previous = ticket.PreviousWrite or Promise.resolve()
	ticket.PreviousWrite = nil
	-- Set once an outcome is being processed, after which a failure must stop the session.
	local settled = false
	local allSuperseded = false
	local superseded = nil
	local sent = patch
	local write = previous
		:andThen(function()
			return Promise.all({ dependency, Promise.all(baselinePromises) })
		end)
		:andThen(function(resolved)
			if self.__status == Status.Disconnected then
				return Promise.reject("The session stopped before this write was sent")
			end
			-- Earlier writes and publications may have replaced or removed a target while
			-- this one waited. Its intent no longer has a target, so sending it would only
			-- earn a conflict or an unknown-instance error for an edit nobody can see.
			local fromIds = if self.__instanceMap then self.__instanceMap.fromIds else {}
			sent, superseded = ServeSession.partitionSupersededOperations(patch, targets, fromIds)
			if not PatchSet.isEmpty(superseded) then
				for _, update in superseded.updated do
					sources[update.id] = nil
					sourceBaselines[update.id] = nil
				end
				if PatchSet.isEmpty(sent) then
					allSuperseded = true
					return nil
				end
				record.patch = sent
				local entry = { status = "Pending", patch = superseded, authority = authority }
				table.insert(self.__syncResults, entry)
				finish("Superseded", supersededResult(superseded), entry)
			end
			structuralWrite = false
			for _, update in sent.updated do
				structuralWrite = structuralWrite or update.changedName ~= nil or update.changedParent ~= nil
			end
			if next(sources) ~= nil then
				self.__sourceHistory:begin(operationId, sources)
			end
			local lineage = resolved[1]
			return self.__apiContext:write(
				sent,
				messageCursor,
				authority,
				operationId,
				lineage[#lineage],
				sourceBaselines
			)
		end)
		:andThen(function(result)
			settled = true
			if allSuperseded then
				-- Nothing was sent, so nothing was acknowledged. Dependents keep the
				-- predecessor lineage, and buffered guards release without a response.
				self:__resolveWriteTicket(ticket)
				forgetDeletions()
				finish("Superseded", supersededResult(superseded))
				return self:__finishLocalProperties(localProperties, nil)
			end
			-- Partial failures grant nothing new. The captured valid predecessor still
			-- proves only its old effects, so missing effects continue to fail closed.
			self:__resolveWriteTicket(
				ticket,
				if ServeSession.getWriteStatus(result) == "Saved" then result.acknowledgement else nil
			)
			if next(sources) ~= nil then
				self.__sourceHistory:consume(self.__apiContext:getMessageCursor())
				self.__sourceHistory:acknowledge(operationId, result.messageCursor)
			end
			self:__acknowledgeSourceDrafts(sources, result)
			forgetDeletions()
			self:__untrackRejectedAdditions(patch, result)
			self:__untrackSavedRemovals(result)
			finish(ServeSession.getWriteStatus(result), result)
			-- Handlers and callbacks must succeed before buffered values can replay.
			local replay = self:__finishLocalProperties(localProperties, result)
			if replay ~= nil then
				return replay:andThen(function()
					return result
				end)
			end
			return result
		end)
		:catch(function(err)
			self:__resolveWriteTicket(ticket)
			forgetDeletions()
			if
				settled
				or structuralWrite
				or (getmetatable(err) == Http.Error and err.type == Http.Error.Kind.Protocol)
			then
				-- Without a validated structural response, replay could destroy unsaved
				-- intent. Like invalid acknowledgements, stop before queued writes run.
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
	write
		:finally(function()
			self:__completeWriteTicket(ticket)
		end)
		:catch(function() end)
	return write
end

function ServeSession:listSourceCheckpoints()
	return self.__apiContext:listSourceCheckpoints()
end

function ServeSession:readSourceCheckpoint(checkpointId)
	return self.__apiContext:readSourceCheckpoint(checkpointId)
end

function ServeSession:discardSourceCheckpoint(checkpointId)
	return self.__apiContext:discardSourceCheckpoint(checkpointId)
end

function ServeSession:restoreSourceCheckpoint(checkpointId, instanceId, expectedSource)
	assert(
		type(instanceId) == "string" and type(expectedSource) == "string",
		"Restoration requires a script identity and its exact current editor text."
	)
	local instance = self.__instanceMap.fromIds[instanceId]
	if instance == nil or not instance:IsA("LuaSourceContainer") then
		return Promise.reject("The recovery target is not a tracked script.")
	end
	local revision = self:__sourceRevision(instanceId, instance)
	return self.__apiContext:readSourceCheckpoint(checkpointId):andThen(function(response)
		local checkpoint = response.checkpoint
		if type(checkpoint) ~= "table" or checkpoint.id ~= instanceId or type(checkpoint.source) ~= "string" then
			return Promise.reject(
				"This checkpoint does not identify the selected script. Read it explicitly instead of replaying it onto another identity."
			)
		end
		if self.__status == Status.Disconnected or self.__instanceMap.fromIds[instanceId] ~= instance then
			return Promise.reject("The recovery target changed while the checkpoint was read.")
		end
		local proposed = false
		self:__updateEditorSource(instance, function()
			proposed = false
			if
				self.__status == Status.Disconnected
				or self.__instanceMap.fromIds[instanceId] ~= instance
				or self:__sourceRevision(instanceId, instance) ~= revision
				or self:__readEditorSource(instance) ~= expectedSource
			then
				return nil
			end
			proposed = true
			return checkpoint.source
		end)
		if not proposed or self:__readEditorSource(instance) ~= checkpoint.source then
			return Promise.reject("The editor changed during recovery. Its newer text was preserved.")
		end
		-- Restoring a draft is a new local edit, not permission to bypass disk authority.
		return { sessionId = response.sessionId, checkpointId = checkpointId, editorRestored = true }
	end)
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

						local chain = self.__lastPublication or Promise.resolve()
						for index, message in messagesPacket.messages do
							local publicationCursor = messagesPacket.messageCursor - #messagesPacket.messages + index
							self:__rememberSourcePublication(message, publicationCursor)
							self:__rememberStructuralPublication(message, publicationCursor)
							chain = chain:andThen(function()
								return self:__applyPatch(message, publicationCursor)
							end)
						end
						self.__lastPublication = chain
							:andThen(function()
								self.__apiContext:setMessageCursor(messagesPacket.messageCursor)
								return Promise.resolve(self:__releaseLocalProperties(messagesPacket.messageCursor))
									:andThen(function()
										self.__sourceHistory:consume(messagesPacket.messageCursor)
									end)
							end)
							:catch(function(err)
								self:__stopInternal(err)
							end)
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
			local reparentSuccess, reparentError = attemptReparent(self.__instanceMap, child, replacement)
			if not reparentSuccess then
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
		local deleteSuccess, deleteError = attemptReparent(self.__instanceMap, oldInstance, nil)
		local replaceSuccess, replaceError = attemptReparent(self.__instanceMap, replacement, oldParent)

		if not (deleteSuccess and replaceSuccess) then
			Log.warn(
				"Could not swap instances {} and {} during sync replacement: {}",
				oldInstance.Name,
				replacement.Name,
				(deleteError or "") .. "\n" .. (replaceError or "")
			)

			-- We need to revert the failed swap to avoid losing the old instance and children.
			for _, child in replacement:GetChildren() do
				attemptReparent(self.__instanceMap, child, oldInstance)
			end
			attemptReparent(self.__instanceMap, oldInstance, oldParent)
			self.__instanceMap:insert(id, oldInstance)

			-- Our replacement should never have existed in the first place, so we can just destroy it.
			replacement:Destroy()
			continue
		end

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
				attemptReparent(self.__instanceMap, child, targetParent)
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

function ServeSession:__notifySourceRecovery(id, checkpointId)
	local result = {
		accepted = { removed_instances = {}, added_instances = {}, updated_instances = {} },
		conflicts = { removed_instances = {}, added_instances = {}, updated_instances = {} },
		errors = {
			{
				id = id,
				property = "Source",
				message = `A filesystem conflict was detected. Your Studio draft was saved in recovery checkpoint "{checkpointId}".`,
			},
		},
	}
	table.insert(self.__syncResults, { status = "Conflicted", result = result })
	if self.__syncResultCallback ~= nil then
		self.__syncResultCallback("Conflicted", result)
	end
end

function ServeSession:__checkpointDisplacedSources(patch, publicationCursor)
	local targets = {}
	local untrackedDocument = false
	local function discover(instance)
		if
			not self.__initialSourceRecovery
			or not instance:IsA("LuaSourceContainer")
			or not self:__hasOpenScriptDocument(instance)
		then
			return
		end
		local id = self.__instanceMap.fromInstances[instance]
		if id == nil then
			untrackedDocument = true
			return
		end
		if self.__sourceDrafts[id] == nil then
			self.__sourceDrafts[id] = {
				Instance = instance,
				Revision = self:__sourceRevision(id, instance),
				Source = self:__readEditorSource(instance),
			}
		end
	end
	for _, update in patch.updated do
		local instance = self.__instanceMap.fromIds[update.id]
		local value = update.changedProperties and update.changedProperties.Source
		if instance and value then
			discover(instance)
			local draft = self.__sourceDrafts[update.id]
			local origin = self.__sourceHistory:classify(
				update.id,
				patch.sourceOperations and patch.sourceOperations[update.id],
				instance,
				self:__sourceRevision(update.id, instance)
			)
			if
				draft
				and draft.Instance == instance
				and origin == "Independent"
				and self:__readEditorSource(instance) ~= value.String
			then
				targets[update.id] = { Draft = draft, Expected = value.String }
			end
		end
	end
	local replaced = {}
	for _, id in patch.removed do
		local instance = if typeof(id) == "Instance" then id else self.__instanceMap.fromIds[id]
		if instance ~= nil then
			replaced[instance] = true
		end
	end
	for _, update in patch.updated do
		local instance = self.__instanceMap.fromIds[update.id]
		if update.changedClassName ~= nil and instance ~= nil and replaced[instance] == nil then
			replaced[instance] = false
		end
	end
	if next(replaced) ~= nil then
		for root, descendants in replaced do
			discover(root)
			if descendants and self.__initialSourceRecovery then
				for _, instance in root:GetDescendants() do
					discover(instance)
				end
			end
		end
		if untrackedDocument then
			return Promise.reject(
				"An open script document would be removed without a managed recovery identity. Its editor text was retained. Copy the draft to a safe file before reconnecting."
			)
		end
		for id, draft in self.__sourceDrafts do
			local instance = self.__instanceMap.fromIds[id]
			if instance == draft.Instance then
				for root, descendants in replaced do
					if instance == root or (descendants and instance:IsDescendantOf(root)) then
						targets[id] = targets[id] or { Draft = draft, Expected = "" }
					end
				end
			end
		end
	end
	local requests = {}
	for id, target in targets do
		local draft = target.Draft
		local source = self:__readEditorSource(draft.Instance)
		if draft.CheckpointSource == source then
			continue
		end
		table.insert(
			requests,
			self.__apiContext
				:createSourceCheckpoint(
					id,
					source,
					target.Expected,
					publicationCursor or self.__apiContext:getMessageCursor()
				)
				:andThen(function(response)
					if self.__status == Status.Disconnected then
						return Promise.reject("The session stopped while securing a Source recovery checkpoint.")
					end
					if
						self.__instanceMap.fromIds[id] ~= draft.Instance
						or self:__readEditorSource(draft.Instance) ~= source
						or self.__sourceDrafts[id] ~= draft
					then
						return Promise.reject(
							"The script changed while its recovery checkpoint was being stored. The current editor draft was retained."
						)
					end
					draft.CheckpointSource = source
					draft.CheckpointId = response.checkpointId
					self:__notifySourceRecovery(id, response.checkpointId)
					return nil
				end)
		)
	end
	return if #requests > 0 then Promise.all(requests) else nil
end

function ServeSession:__applyPatch(patch, publicationCursor)
	if publicationCursor ~= nil then
		self:__rememberSourcePublication(patch, publicationCursor)
		self:__rememberStructuralPublication(patch, publicationCursor)
	end
	local function apply()
		if self.__status == Status.Disconnected then
			return Promise.reject("The session stopped before applying this publication.")
		end
		return self:__applyPatchNow(self:__filterSourceEchoes(patch), publicationCursor)
	end
	local pending = self.__pendingSourcePatch
	local success, result = pcall(function()
		return if pending ~= nil then pending:andThen(apply) else apply()
	end)
	if not success then
		self:__stopInternal(result)
		return Promise.reject(result)
	end
	if result == nil then
		return nil
	end
	local token = {}
	self.__pendingSourcePatchToken = token
	local guarded = result:catch(function(err)
		self:__stopInternal(err)
		return Promise.reject(err)
	end)
	self.__pendingSourcePatch = guarded
	guarded
		:finally(function()
			if self.__pendingSourcePatchToken == token then
				self.__pendingSourcePatch = nil
				self.__pendingSourcePatchToken = nil
			end
		end)
		:catch(function() end)
	return guarded
end

function ServeSession:__applyPatchNow(patch, publicationCursor, prepared)
	local cursors = {}
	for id in patch.added do
		cursors[id] = publicationCursor
	end
	for _, update in patch.updated do
		local replay = patch.sourceReplays and patch.sourceReplays[update.id]
		cursors[update.id] = if replay then replay.Cursor else publicationCursor
	end
	local operations = { Cursors = cursors }
	local thread = coroutine.running()
	local previous = self.__sourceApplyBatches[thread]
	self.__sourceApplyBatches[thread] = operations
	local success, result = pcall(self.__applyPatchBody, self, patch, publicationCursor, prepared)
	self.__sourceApplyBatches[thread] = previous
	if not success then
		error(result, 0)
	end
	if result ~= nil then
		return result:andThen(function()
			return self:__awaitSourceOperations(operations)
		end)
	end
	return self:__awaitSourceOperations(operations)
end

function ServeSession:__applyPatchBody(patch, publicationCursor, prepared)
	self:__keepUndoneDeletions(patch)
	patch = self:__protectLocalProperties(patch, publicationCursor)
	local patchTimestamp = DateTime.now():FormatLocalTime("LTS", "en-us")
	local historyRecording =
		ChangeHistoryService:TryBeginRecording(ServeSession.PATCH_RECORDING_PREFIX .. patchTimestamp)
	if not historyRecording then
		-- There can only be one recording at a time
		Log.debug("Failed to begin history recording for " .. patchTimestamp .. ". Another recording is in progress.")
	end

	if not prepared then
		Timer.start("precommitCallbacks", "wall")
		-- Hooks may yield or edit documents. Revalidate intent after they return.
		for _, callback in self.__precommitCallbacks do
			local success, err = pcall(callback, patch, self.__instanceMap)
			if not success then
				Log.warn("Precommit hook errored: {}", err)
			end
		end
		Timer.stop()
	end
	patch = self:__protectLocalProperties(patch, publicationCursor)
	local checkpoint = self:__checkpointDisplacedSources(patch, publicationCursor)
	if checkpoint ~= nil then
		if historyRecording then
			ChangeHistoryService:FinishRecording(historyRecording, Enum.FinishRecordingOperation.Commit)
		end
		return checkpoint:andThen(function()
			return self:__applyPatchNow(patch, publicationCursor, true)
		end)
	end

	local addedBefore = {}
	for id in patch.added do
		addedBefore[id] = self.__instanceMap.fromIds[id] or false
	end
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

	-- A metadata-only context refresh may change authored/emitted transforms without
	-- changing bytes. It can advance an unchanged, proven document, never an older
	-- captured draft, a suppressed echo, or a pending/failed Source proposal.
	if publicationCursor ~= nil and self.__status ~= Status.Disconnected then
		local failedSource = {}
		for id in unappliedPatch.added do
			failedSource[id] = true
		end
		local failed = {}
		for _, update in unappliedPatch.updated do
			if update.changedProperties and update.changedProperties.Source ~= nil then
				failedSource[update.id] = true
			end
			if update.changedMetadata ~= nil then
				failed[update.id] = true
			end
		end
		-- Reification can finish before the new instance's observer is attached.
		-- Only a newly created, successfully matched document gets this baseline;
		-- existing/unapplied documents must use their guarded terminal outcome.
		for id, virtual in patch.added do
			local instance = self.__instanceMap.fromIds[id]
			local source = virtual.Properties and virtual.Properties.Source
			if
				instance ~= nil
				and instance ~= addedBefore[id]
				and source ~= nil
				and not failedSource[id]
				and self.__sourcePendingCounts[instance] == nil
				and self:__sourceRevision(id, instance) == 0
				and instance:IsA("LuaSourceContainer")
				and self:__readEditorSource(instance) == source.String
			then
				self.__sourceBaselines[instance] = { Cursor = publicationCursor, Revision = 0 }
			end
		end
		for _, update in patch.updated do
			local instance = self.__instanceMap.fromIds[update.id]
			local baseline = instance and self.__sourceBaselines[instance]
			if
				update.changedMetadata ~= nil
				and not update.sourceSuppressed
				and not failed[update.id]
				and (update.changedProperties == nil or update.changedProperties.Source == nil)
				and baseline ~= nil
				and self.__sourcePendingCounts[instance] == nil
				and baseline.Promise == nil
				and baseline.Cursor ~= nil
				and publicationCursor > baseline.Cursor
				and baseline.Revision == self:__sourceRevision(update.id, instance)
				and self.__sourceDrafts[update.id] == nil
				and not (self.__localPropertyEdits[update.id] and self.__localPropertyEdits[update.id].Source)
			then
				self.__sourceBaselines[instance] = { Cursor = publicationCursor, Revision = baseline.Revision }
			end
		end
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

	-- Fallback reconciliation can replace identity even for a scalar-only packet.
	self:__pruneStructuralGuards()
	local structural = #patch.removed > 0
	for _, update in patch.updated do
		structural = structural or update.changedClassName ~= nil
	end
	if structural then
		for id, revision in self.__sourceRevisions do
			if self.__instanceMap.fromIds[id] ~= revision.Instance then
				self.__sourceRevisions[id] = nil
			end
		end
		for id, draft in self.__sourceDrafts do
			if self.__instanceMap.fromIds[id] ~= draft.Instance then
				self.__sourceDrafts[id] = nil
			end
		end
	end
	if historyRecording then
		ChangeHistoryService:FinishRecording(historyRecording, Enum.FinishRecordingOperation.Commit)
	end
	return nil
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

		-- This is the only hydration-wide baseline pass. Mere map hydration is not
		-- Source consumption, especially for open documents with older unsaved text.
		for id, virtual in readResponseBody.instances do
			local instance = self.__instanceMap.fromIds[id]
			local source = virtual.Properties and virtual.Properties.Source
			if instance ~= nil and source ~= nil and instance:IsA("LuaSourceContainer") then
				local revision = self:__sourceRevision(id, instance)
				if self:__readEditorSource(instance) == source.String then
					self.__sourceBaselines[instance] = { Cursor = readResponseBody.messageCursor, Revision = revision }
				end
			end
			step()
		end
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
			self.__initialSourceRecovery = true
			return Promise.resolve(self:__applyPatch(catchUpPatch, readResponseBody.messageCursor)):finally(function()
				self.__initialSourceRecovery = nil
			end)
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
	for ticket in table.clone(self.__writeTickets or {}) do
		local resolve, complete = ticket.Resolve, ticket.Complete
		ticket.Parent, ticket.Resolve, ticket.Dependency, ticket.PreviousWrite, ticket.Complete =
			nil, nil, nil, nil, nil
		if resolve ~= nil then
			resolve({})
		end
		if complete ~= nil then
			complete()
		end
	end
	self.__writeTickets = {}
	self.__writeHead, self.__pendingWriteDependencies = nil, nil
	for _, pending in self.__remoteSources do
		if pending.BaselineToken ~= nil then
			self:__resolveSourceBaseline(pending.BaselineToken)
		end
	end
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
