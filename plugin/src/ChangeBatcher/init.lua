--[[
	The ChangeBatcher is responsible for collecting and dispatching changes made
	to tracked instances during two-way sync.
]]

local RunService = game:GetService("RunService")
local SerializationService = game:GetService("SerializationService")

local PatchSet = require(script.Parent.PatchSet)

local createAdditions = require(script.createAdditions)
local createPatchSet = require(script.createPatchSet)

local function serialize(instance)
	return SerializationService:SerializeInstancesAsync({ instance })
end

local ChangeBatcher = {}
ChangeBatcher.__index = ChangeBatcher

local BATCH_INTERVAL = 0.2

-- How long a known instance may stay unparented before it is sent as a deletion.
-- Studio detaches instances during drags, cut and paste, and deletions that are
-- later undone. Sending those as deletions removes their files, and reattaching
-- then sends a fresh copy with new ids, so a move becomes a delete and a create.
local DETACH_GRACE = 3

--[[
	`onBatchTaken`, when given, is called as each batch is taken out, before
	anything yields, and whatever it returns is passed to `onChangesFlushed`
	with that batch. A session uses it to keep the message cursor each batch
	started from, since later edits start a new batch while one is prepared.
	`onBatchPrepared` also sees empty batches, or nil on a preparation error, so
	omitted properties do not leave session guards waiting for a write never sent.
	`onDetachmentExpired`, when given, is called with an instance and its held
	property names once it stayed unparented for the whole grace period, so the
	session can queue its deletion like any other change.
	`batchContext`, when given, takes causal metadata before holding changes and
	merges it without mutating either input. Held changes retain that metadata
	until release; only batches ready for encoding call `onBatchTaken`.
]]
function ChangeBatcher.new(
	instanceMap,
	onChangesFlushed,
	onBatchTaken,
	onBatchPrepared,
	onDetachmentExpired,
	batchContext
)
	local self

	local renderSteppedConnection = RunService.Heartbeat:Connect(function(dt)
		self:__cycle(dt)
	end)

	self = setmetatable({
		__accumulator = 0,
		__renderSteppedConnection = renderSteppedConnection,
		__instanceMap = instanceMap,
		__onChangesFlushed = onChangesFlushed,
		__onBatchTaken = onBatchTaken,
		__onBatchPrepared = onBatchPrepared,
		__onDetachmentExpired = onDetachmentExpired,
		__batchContext = batchContext,
		__pendingPropertyChanges = {},
		__pendingAdditions = {},
		__detached = {},
		__detachGrace = DETACH_GRACE,
		__clock = os.clock,
		__serialize = serialize,
	}, ChangeBatcher)

	return self
end

function ChangeBatcher:stop()
	self.__renderSteppedConnection:Disconnect()
	self.__pendingPropertyChanges = {}
	self.__pendingAdditions = {}
	self.__detached = {}
	self.__pendingContext = nil
	self.__stopped = true
end

function ChangeBatcher:__mergeContext(previous, current)
	return if self.__batchContext then self.__batchContext.merge(previous, current) else nil
end

-- Merges held property names and their causal context into the pending batch.
function ChangeBatcher:__mergePending(instance, properties, context)
	self.__pendingContext = self:__mergeContext(self.__pendingContext, context)
	local pending = self.__pendingPropertyChanges[instance]
	if pending == nil then
		pending = {}
		self.__pendingPropertyChanges[instance] = pending
	end
	for name in properties do
		pending[name] = true
	end
end

--[[
	Ends held detachments. A reattached instance already queued its own Parent
	change, which is encoded as a move, so only its other held properties are
	carried over. One unparented for the whole grace period is released as a
	deletion. These changes are ready to encode and must not be held again.
]]
function ChangeBatcher:__releaseDetachments()
	local now = self.__clock()
	for instance, held in self.__detached do
		if self.__instanceMap.fromInstances[instance] == nil then
			-- The server removed or replaced it meanwhile, so there is nothing to send.
			self.__detached[instance] = nil
		elseif instance.Parent ~= nil then
			self.__detached[instance] = nil
			self:__mergePending(instance, held.properties, held.context)
		elseif now - held.since >= self.__detachGrace then
			self.__detached[instance] = nil
			if self.__onDetachmentExpired then
				self.__pendingContext = self:__mergeContext(self.__pendingContext, held.context)
				self.__onDetachmentExpired(instance, held.properties)
			else
				self:__mergePending(instance, held.properties, held.context)
			end
		end
	end
end

-- Takes newly unparented known instances out of `changes` and holds them.
function ChangeBatcher:__holdDetachments(changes, context)
	if self.__detachGrace <= 0 then
		return
	end
	local now = self.__clock()
	for instance, properties in changes do
		if
			(properties.Parent or self.__detached[instance] ~= nil)
			and instance.Parent == nil
			and self.__instanceMap.fromInstances[instance] ~= nil
		then
			local held = self.__detached[instance]
			if held == nil then
				held = { since = now, properties = {} }
				self.__detached[instance] = held
			end
			held.context = self:__mergeContext(held.context, context)
			for name in properties do
				held.properties[name] = true
			end
			changes[instance] = nil
		end
	end
end

function ChangeBatcher:isEmpty()
	return next(self.__pendingPropertyChanges) == nil and next(self.__pendingAdditions) == nil
end

-- Queues an instance that appeared under a known parent. Whether it still
-- needs sending is decided when the batch is sent.
function ChangeBatcher:addChild(instance)
	self.__pendingAdditions[instance] = true
end

function ChangeBatcher:add(instance, propertyName)
	local properties = self.__pendingPropertyChanges[instance]

	if not properties then
		properties = {}
		self.__pendingPropertyChanges[instance] = properties
	end

	properties[propertyName] = true
end

function ChangeBatcher:__cycle(dt)
	self.__accumulator += dt

	if self.__accumulator >= BATCH_INTERVAL then
		self.__accumulator -= BATCH_INTERVAL

		self:__flush()
	end

	self.__instanceMap:unpauseAllInstances()
end

function ChangeBatcher:__flush()
	if self.__suspended or self.__stopped then
		return nil
	end
	-- Serializing new instances yields, so a flush can still be running when
	-- the next one is due. Only one runs at a time, which keeps writes in order
	-- and stops two flushes from sending the same instances.
	if self.__flushing then
		return nil
	end
	local captured
	if not self:isEmpty() then
		-- Separate fresh held intent before released changes join this batch. A new
		-- hold must not inherit an unrelated release's cursor or dependencies.
		captured = if self.__batchContext then self.__batchContext.take() else nil
		self:__holdDetachments(self.__pendingPropertyChanges, captured)
		if self:isEmpty() then
			-- No fresh operations will be emitted, so their context cannot constrain
			-- the released operations either. Each hold already retained its own.
			captured = nil
		end
	end
	self:__releaseDetachments()
	if self:isEmpty() then
		return nil
	end

	local batchContext = self:__mergeContext(self.__pendingContext, captured)
	self.__pendingContext = nil
	-- Changes made while preparation yields belong to the next batch.
	local candidates = self.__pendingAdditions
	local changes = self.__pendingPropertyChanges
	self.__pendingAdditions = {}
	self.__pendingPropertyChanges = {}
	self.__flushing = true
	local context = if self.__onBatchTaken then self.__onBatchTaken(changes, batchContext) else nil

	-- New instances are known before the other changes are encoded, so moving
	-- an existing instance into one in the same batch is a move to a known parent.
	local ok, patch = pcall(function()
		local additions, references = createAdditions(self.__instanceMap, candidates, self.__serialize)
		-- References a new instance's model couldn't hold are sent as edits.
		for _, reference in references do
			local properties = changes[reference[1]] or {}
			properties[reference[2]] = true
			changes[reference[1]] = properties
		end
		local batch = createPatchSet(self.__instanceMap, changes)
		batch.added = additions
		return batch
	end)
	self.__flushing = false
	-- Even an empty or failed preparation must release guards for omitted properties.
	if self.__onBatchPrepared then
		self.__onBatchPrepared(if ok then patch else nil, context)
	end
	if not ok then
		error(patch, 0)
	end

	-- A session stopped while this batch was prepared sends nothing.
	if self.__suspended or self.__stopped or PatchSet.isEmpty(patch) then
		return nil
	end

	return self.__onChangesFlushed(patch, context)
end

return ChangeBatcher
