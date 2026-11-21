local Packages = script.Parent.Parent.Parent.Packages
local Log = require(Packages.Log)
local encodeProperty = require(script.Parent.encodeProperty)
local getWritablePropertyDescriptor = require(script.Parent.getWritablePropertyDescriptor)

return function(instance, instanceId, properties, instanceMap)
	local update = {
		id = instanceId,
		changedProperties = {},
	}

	for propertyName in properties do
		if propertyName == "Name" then
			update.changedName = instance.Name
		elseif propertyName == "Parent" then
			update.changedParent = instanceMap.fromInstances[instance.Parent]
		else
			local descriptor = getWritablePropertyDescriptor(instance.ClassName, propertyName)
			if descriptor == nil then
				Log.debug("Could not sync back property {:?}.{}", instance, propertyName)
				continue
			end

			if update.changedProperties[descriptor.name] ~= nil then
				continue
			end
			local encodeSuccess, encodeResult = encodeProperty(instance, propertyName, descriptor, instanceMap)

			if not encodeSuccess then
				Log.debug("Could not sync back property {:?}.{}: {}", instance, propertyName, encodeResult)
				continue
			end

			update.changedProperties[descriptor.name] = encodeResult
		end
	end

	if next(update.changedProperties) == nil and update.changedName == nil and update.changedParent == nil then
		return nil
	end

	return update
end
