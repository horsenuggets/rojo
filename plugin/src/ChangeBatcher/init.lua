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

function ChangeBatcher.new(instanceMap, onChangesFlushed)
	local self

	local renderSteppedConnection = RunService.Heartbeat:Connect(function(dt)
		self:__cycle(dt)
	end)

	self = setmetatable({
		__accumulator = 0,
		__renderSteppedConnection = renderSteppedConnection,
		__instanceMap = instanceMap,
		__onChangesFlushed = onChangesFlushed,
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
	if self:isEmpty() then
		return nil
	end

	-- New instances are known before the other changes are encoded, so moving
	-- an existing instance into one in the same batch is a move to a known parent.
	local additions = createAdditions(self.__instanceMap, self.__pendingAdditions, self.__serialize)
	local patch = createPatchSet(self.__instanceMap, self.__pendingPropertyChanges)
	patch.added = additions

	if PatchSet.isEmpty(patch) then
		return nil
	end

	return self.__onChangesFlushed(patch)
end

return ChangeBatcher
