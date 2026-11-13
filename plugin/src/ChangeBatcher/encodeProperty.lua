local Packages = script.Parent.Parent.Parent.Packages
local Log = require(Packages.Log)
local RbxDom = require(Packages.RbxDom)

local unmanagedAttributes = require(script.Parent.Parent.Reconciler.unmanagedAttributes)

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

	-- Attributes the filesystem can't hold, like infinity, would otherwise be
	-- encoded as sentinel values and saved. The server keeps its own copies.
	if dataType == "Attributes" and type(readResult) == "table" then
		local managed = {}
		for name, value in readResult do
			if not unmanagedAttributes.isUnmanaged(name, value) then
				managed[name] = value
			end
		end
		readResult = managed
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
