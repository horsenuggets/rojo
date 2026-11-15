--[[
	Take an InstanceMap and a dictionary mapping instances to sets of property
	names. Populate a patch with the encoded values of all the given properties
	on all the given instances (or, if any changes set Parent to nil, removals
	of instances) and return the patch.
]]

local Packages = script.Parent.Parent.Parent.Packages
local Log = require(Packages.Log)

local PatchSet = require(script.Parent.Parent.PatchSet)

local encodePatchUpdate = require(script.Parent.encodePatchUpdate)

return function(instanceMap, propertyChanges)
	local patch = PatchSet.newEmpty()

	for instance, properties in propertyChanges do
		local instanceId = instanceMap.fromInstances[instance]

		-- The DataModel comes from the project file rather than anything Studio can
		-- write back, and connecting renames it to the project's name.
		if instanceId == nil or instance:IsA("DataModel") then
			propertyChanges[instance] = nil
			continue
		end

		if properties.Parent and instance.Parent == nil then
			table.insert(patch.removed, instanceId)
		else
			if properties.Parent and instanceMap.fromInstances[instance.Parent] == nil then
				-- An unmanaged destination has no server identity. Undo only the move,
				-- without feeding our restoration back into the batcher.
				local wasPaused = instanceMap.pausedUpdateInstances[instance]
				instanceMap:pauseInstance(instance)
				local success, err = pcall(function()
					instance.Parent = instanceMap.knownParents[instance]
					instanceMap.observedParents[instance] = instance.Parent
				end)
				if not wasPaused then
					instanceMap:unpauseInstance(instance)
				end
				properties.Parent = nil
				if not success then
					Log.warn("Could not restore the authoritative parent of {:?}. {}", instance, err)
				end
			end

			local update = encodePatchUpdate(instance, instanceId, properties, instanceMap)
			if update ~= nil then
				table.insert(patch.updated, update)
			end
		end

		propertyChanges[instance] = nil
	end

	return patch
end
