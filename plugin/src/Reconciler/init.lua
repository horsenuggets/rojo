--[[
	This module defines the meat of the Rojo plugin and how it manages tracking
	and mutating the Roblox DOM.
]]

local Rojo = script:FindFirstAncestor("Rojo")
local Plugin = Rojo.Plugin

local Timer = require(Plugin.Timer)
local yieldBudget = require(Plugin.yieldBudget)

local applyPatch = require(script.applyPatch)
local hydrate = require(script.hydrate)
local diff = require(script.diff)

local Reconciler = {}
Reconciler.__index = Reconciler

function Reconciler.new(instanceMap)
	local self = {
		-- Tracks all of the instances known by the reconciler by ID.
		__instanceMap = instanceMap,
	}

	return setmetatable(self, Reconciler)
end

function Reconciler:applyPatch(patch)
	Timer.start("Reconciler:applyPatch")

	local unappliedPatch = applyPatch(self.__instanceMap, patch)

	Timer.stop()
	return unappliedPatch
end

-- Hydrating and diffing walk the whole place. On a large one that outlasts
-- Studio's script timeout, which stops the sync without an error to report and
-- leaves the session connecting forever, so both yield as they go.
function Reconciler:hydrate(virtualInstances, rootId, rootInstance)
	Timer.start("Reconciler:hydrate")
	local result = hydrate(self.__instanceMap, virtualInstances, rootId, rootInstance, yieldBudget())
	Timer.stop()

	return result
end

function Reconciler:diff(virtualInstances, rootId)
	Timer.start("Reconciler:diff")
	local success, result = diff(self.__instanceMap, virtualInstances, rootId, yieldBudget())
	Timer.stop()

	return success, result
end

return Reconciler
