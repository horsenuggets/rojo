local Packages = script.Parent.Parent.Parent.Packages
local Log = require(Packages.Log)
local RbxDom = require(Packages.RbxDom)

local unmanagedAttributes = require(script.Parent.Parent.Reconciler.unmanagedAttributes)

local function isNonFinite(value)
	return type(value) == "number" and (value ~= value or value == math.huge or value == -math.huge)
end

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

	-- Attributes the filesystem can't hold, like a Vector3 with a NaN component,
	-- would otherwise be encoded and saved. The server keeps its own copies.
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

	-- rbx_dom_lua stands in a large finite number for infinity, but msgpack holds
	-- it exactly and the filesystem writes it in a tagged form, so send it as is.
	if (dataType == "Float32" or dataType == "Float64") and isNonFinite(readResult) then
		encodeResult = { [dataType] = readResult }
	elseif dataType == "Attributes" and type(readResult) == "table" then
		for name, value in readResult do
			if isNonFinite(value) then
				encodeResult.Attributes[name] = { Float64 = value }
			end
		end
	end

	return true, encodeResult
end
