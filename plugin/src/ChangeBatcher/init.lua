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

--[[
	`onBatchTaken`, when given, is called as each batch is taken out, before
	anything yields, and whatever it returns is passed to `onChangesFlushed`
	with that batch. A session uses it to keep the message cursor each batch
	started from, since later edits start a new batch while one is prepared.
]]
function ChangeBatcher.new(instanceMap, onChangesFlushed, onBatchTaken)
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
		__pendingPropertyChanges = {},
		__pendingAdditions = {},
		__serialize = serialize,
	}, ChangeBatcher)

	return self
end

function ChangeBatcher:stop()
	self.__renderSteppedConnection:Disconnect()
	self.__pendingPropertyChanges = {}
	self.__pendingAdditions = {}
	self.__stopped = true
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
	-- Serializing new instances yields, so a flush can still be running when
	-- the next one is due. Only one runs at a time, which keeps writes in order
	-- and stops two flushes from sending the same instances.
	if self.__flushing or self:isEmpty() then
		return nil
	end
	self.__flushing = true

	-- This batch is taken out first, so changes made while it's prepared go to
	-- the next one.
	local candidates = self.__pendingAdditions
	local changes = self.__pendingPropertyChanges
	self.__pendingAdditions = {}
	self.__pendingPropertyChanges = {}
	local context = if self.__onBatchTaken then self.__onBatchTaken() else nil

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
	if not ok then
		error(patch, 0)
	end

	-- A session stopped while this batch was prepared sends nothing.
	if self.__stopped or PatchSet.isEmpty(patch) then
		return nil
	end

	return self.__onChangesFlushed(patch, context)
end

return ChangeBatcher
