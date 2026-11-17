--[[
	This module defines the meat of the Rojo plugin and how it manages tracking
	and mutating the Roblox DOM.
]]

local Rojo = script:FindFirstAncestor("Rojo")
local Plugin = Rojo.Plugin

local Timer = require(Plugin.Timer)

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

-- Hydrating and diffing walk the whole place, and call `step` as they go so
-- the caller can yield or stop the walk by raising an error. The timer is
-- stopped either way, so a cancelled walk doesn't leave it running.
function Reconciler:hydrate(virtualInstances, rootId, rootInstance, step)
	Timer.start("Reconciler:hydrate")
	local ok, result = pcall(hydrate, self.__instanceMap, virtualInstances, rootId, rootInstance, step)
	Timer.stop()

	if not ok then
		error(result, 0)
	end
	return result
end

function Reconciler:diff(virtualInstances, rootId, step)
	Timer.start("Reconciler:diff")
	local ok, success, result = pcall(diff, self.__instanceMap, virtualInstances, rootId, step)
	Timer.stop()

	if not ok then
		error(success, 0)
	end
	return success, result
end

return Reconciler
