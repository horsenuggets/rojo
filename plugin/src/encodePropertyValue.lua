--[[

encodePropertyValue

Encodes a property value without imposing syncback ownership rules. Keeps numeric
components exact where rbx_dom_lua substitutes finite values for infinity.

--]]

const RbxDom = require(script.Parent.Parent.Packages.RbxDom)
const msgpack = require("../Packages/msgpack")
const function encodeCapabilities(value)
	if typeof(value) ~= "SecurityCapabilities" then
		return false, "Security capabilities must be a SecurityCapabilities value."
	end
	local high = 0
	local low = 0
	local reconstructed = SecurityCapabilities.new()
	for _, item in Enum.SecurityCapability:GetEnumItems() do
		-- Deprecated enum members can construct a different, canonical capability.
		const singleton = SecurityCapabilities.new(item)
		if tostring(singleton) == item.Name and value:Contains(item) then
			const bit = item.Value
			if bit < 0 or bit >= 64 or bit % 1 ~= 0 then
				return false, `Security capability "{item.Name}" has an unrepresentable bit index.`
			end
			if bit < 32 then
				low = bit32.bor(low, bit32.lshift(1, bit))
			else
				high = bit32.bor(high, bit32.lshift(1, bit - 32))
			end
			reconstructed = reconstructed:Add(item)
		end
	end
	if reconstructed ~= value then
		return false, "Security capabilities contain bits that the public enum cannot represent."
	end
	-- Http patches UInt64.new into a lossy number decoder. Use the tagged wire
	-- representation directly to retain both words as an unsigned MessagePack integer.
	return true,
		{
			SecurityCapabilities = {
				_msgpackType = msgpack.UInt64,
				mostSignificantPart = high,
				leastSignificantPart = low,
			},
		}
end
const function vector2(vector)
	return { vector.X, vector.Y }
end
const function vector3(vector)
	return { vector.X, vector.Y, vector.Z }
end
const function number(value)
	return value
end
const exactEncoders = {
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
const function encodeExactly(value, encoded)
	const ty = next(encoded)
	const encoder = exactEncoders[ty]
	return if encoder == nil then encoded else { [ty] = encoder(value) }
end

return function(value, dataType)
	assert(type(dataType) == "string", "Property data types must be strings.")
	if dataType == "SecurityCapabilities" then
		return encodeCapabilities(value)
	end
	local success, encoded = RbxDom.EncodedValue.encode(value, dataType)
	if not success then
		return false, encoded
	end
	if dataType == "Attributes" and type(value) == "table" then
		for name, attribute in value do
			if encoded.Attributes[name] ~= nil then
				encoded.Attributes[name] = encodeExactly(attribute, encoded.Attributes[name])
			end
		end
	else
		encoded = encodeExactly(value, encoded)
	end
	return true, encoded
end
