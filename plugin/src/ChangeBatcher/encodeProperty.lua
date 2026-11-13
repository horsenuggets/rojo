local Packages = script.Parent.Parent.Parent.Packages
local Log = require(Packages.Log)
local RbxDom = require(Packages.RbxDom)

return function(instance, propertyName, propertyDescriptor, instanceMap)
	local readSuccess, readResult = propertyDescriptor:read(instance)

	if not readSuccess then
		Log.warn("Could not sync back property {:?}.{}: {}", instance, propertyName, readResult)
		return false, nil
	end

	local dataType = propertyDescriptor.dataType
	if dataType == "Ref" then
		if readResult == nil then
			return true, { Ref = "00000000000000000000000000000000" }
		end

		local referentId = instanceMap and instanceMap.fromInstances[readResult]
		if referentId == nil then
			Log.warn("Cannot sync reference {:?}.{} because its target is not managed by Rojo", instance, propertyName)
			return false, nil
		end

		return true, { Ref = referentId }
	end

	local callSuccess, encodeSuccess, encodeResult = pcall(RbxDom.EncodedValue.encode, readResult, dataType)

	if not callSuccess then
		Log.warn("Could not sync back property {:?}.{}: {}", instance, propertyName, encodeSuccess)
		return false, nil
	end

	if not encodeSuccess then
		Log.warn("Could not sync back property {:?}.{}: {}", instance, propertyName, encodeResult)
		return false, nil
	end

	return true, encodeResult
end
