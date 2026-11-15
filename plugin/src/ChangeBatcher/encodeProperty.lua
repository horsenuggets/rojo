local Packages = script.Parent.Parent.Parent.Packages
local Log = require(Packages.Log)
local RbxDom = require(Packages.RbxDom)

local unmanagedAttributes = require(script.Parent.Parent.Reconciler.unmanagedAttributes)

local function vector2(vector)
	return { vector.X, vector.Y }
end

local function vector3(vector)
	return { vector.X, vector.Y, vector.Z }
end

local function number(value)
	return value
end

-- rbx_dom_lua stands in a large finite number for infinity in these types,
-- including the vectors inside a Ray or Rect. msgpack holds infinity exactly
-- and the filesystem writes it in a tagged form, so they're encoded from
-- their raw components instead, which is the same encoding for finite values.
local exactEncoders = {
	Float32 = number,
	Float64 = number,
	Vector2 = vector2,
	Vector3 = vector3,
	Ray = function(ray)
		return { origin = vector3(ray.Origin), direction = vector3(ray.Direction) }
	end,
	Rect = function(rect)
		return { vector2(rect.Min), vector2(rect.Max) }
	end,
}

local function encodeExactly(value, encoded)
	local ty = next(encoded)
	local encoder = exactEncoders[ty]
	if encoder == nil then
		return encoded
	end
	return { [ty] = encoder(value) }
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
				encodedAttributes[name] = encodeExactly(value, encodedAttributes[name])
			end
		end
	else
		encodeResult = encodeExactly(readResult, encodeResult)
	end

	return true, encodeResult
end
