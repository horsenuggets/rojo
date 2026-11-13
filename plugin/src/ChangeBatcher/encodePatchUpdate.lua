local Packages = script.Parent.Parent.Parent.Packages
local Log = require(Packages.Log)
local RbxDom = require(Packages.RbxDom)
local ReflectionDatabase = require(Packages.RbxDom.database)

local encodeProperty = require(script.Parent.encodeProperty)

return function(instance, instanceId, properties, instanceMap)
	local update = {
		id = instanceId,
		changedProperties = {},
	}

	for propertyName in properties do
		if propertyName == "Name" then
			update.changedName = instance.Name
		else
			local descriptor = RbxDom.findCanonicalPropertyDescriptor(instance.ClassName, propertyName)

			if not descriptor then
				Log.debug("Could not sync back property {:?}.{}", instance, propertyName)
				continue
			end

			if
				descriptor.scriptability == "None"
				or descriptor.scriptability == "Read"
				or descriptor.scriptability == "Write"
			then
				continue
			end

			local canonicalProperty = ReflectionDatabase.Classes[descriptor.className].Properties[descriptor.name]
			if canonicalProperty.Kind.Canonical.Serialization == "DoesNotSerialize" then
				continue
			end

			local encodeSuccess, encodeResult = encodeProperty(instance, propertyName, descriptor, instanceMap)

			if not encodeSuccess then
				Log.debug("Could not sync back property {:?}.{}: {}", instance, propertyName, encodeResult)
				continue
			end

			update.changedProperties[propertyName] = encodeResult
		end
	end

	if next(update.changedProperties) == nil and update.changedName == nil then
		return nil
	end

	return update
end
