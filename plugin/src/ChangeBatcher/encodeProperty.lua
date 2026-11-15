local Packages = script.Parent.Parent.Parent.Packages
local Log = require(Packages.Log)
local RbxDom = require(Packages.RbxDom)

local unmanagedAttributes = require(script.Parent.Parent.Reconciler.unmanagedAttributes)

local function isNonFinite(value)
	return type(value) == "number" and (value ~= value or value == math.huge or value == -math.huge)
end

-- rbx_dom_lua stands in a large finite number for infinity in numbers,
-- Vector2s, and Vector3s, but msgpack holds it exactly and the filesystem writes
-- it in a tagged form, so those values are sent as they are.
local function restoreNonFinite(value, encoded)
	local ty = next(encoded)
	if (ty == "Float32" or ty == "Float64") and isNonFinite(value) then
		return { [ty] = value }
	elseif ty == "Vector2" and (isNonFinite(value.X) or isNonFinite(value.Y)) then
		return { Vector2 = { value.X, value.Y } }
	elseif ty == "Vector3" and (isNonFinite(value.X) or isNonFinite(value.Y) or isNonFinite(value.Z)) then
		return { Vector3 = { value.X, value.Y, value.Z } }
	end
	return encoded
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
			Log.warn("Cannot sync reference {:?}.{} because its target is not managed by Rojo+", instance, propertyName)
			return false, nil
		end

		return true, { Ref = referentId }
	end

	-- Attributes the filesystem doesn't hold, like `RBX` ones, would otherwise
	-- be encoded and saved. The server keeps its own copies.
	if dataType == "Attributes" and type(readResult) == "table" then
		local managed = {}
		for name, value in readResult do
			if not unmanagedAttributes.isUnmanaged(name) then
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

	if dataType == "Attributes" and type(readResult) == "table" then
		local encodedAttributes = encodeResult.Attributes
		for name, value in readResult do
			-- Attributes rbx_dom_lua couldn't encode were already warned about.
			if encodedAttributes[name] ~= nil then
				encodedAttributes[name] = restoreNonFinite(value, encodedAttributes[name])
			end
		end
	else
		encodeResult = restoreNonFinite(readResult, encodeResult)
	end

	return true, encodeResult
end
