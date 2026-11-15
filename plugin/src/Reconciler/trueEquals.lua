--[[
	Fuzzy value-equality used to compare a decoded virtual property value against
	the live value read from a real instance. Shared by `diff` (to decide whether
	a property changed) and `hydrate` (to score candidate instances).
]]

-- Compares numbers with an epsilon of 0.0001 to avoid floating point inequality.
-- NaN equals NaN, since the filesystem stores it like any other value, and
-- infinity only equals itself.
local function numberEq(a: number, b: number): boolean
	return a == b or (a ~= a and b ~= b) or math.abs(a - b) < 0.0001
end

-- The numbers that make up a composite value, in a fixed order, or nil for
-- types that aren't compared component by component.
local function components(value): { number }?
	local valueType = typeof(value)
	if valueType == "Vector2" then
		return { value.X, value.Y }
	elseif valueType == "Vector3" then
		return { value.X, value.Y, value.Z }
	elseif valueType == "CFrame" then
		return { value:GetComponents() }
	elseif valueType == "NumberRange" then
		return { value.Min, value.Max }
	elseif valueType == "UDim" then
		return { value.Scale, value.Offset }
	elseif valueType == "UDim2" then
		return { value.X.Scale, value.X.Offset, value.Y.Scale, value.Y.Offset }
	elseif valueType == "Rect" then
		return { value.Min.X, value.Min.Y, value.Max.X, value.Max.Y }
	elseif valueType == "Ray" then
		local origin, direction = value.Origin, value.Direction
		return { origin.X, origin.Y, origin.Z, direction.X, direction.Y, direction.Z }
	elseif valueType == "NumberSequence" then
		local numbers = {}
		for _, keypoint in value.Keypoints do
			table.insert(numbers, keypoint.Time)
			table.insert(numbers, keypoint.Value)
			table.insert(numbers, keypoint.Envelope)
		end
		return numbers
	elseif valueType == "ColorSequence" then
		local numbers = {}
		for _, keypoint in value.Keypoints do
			table.insert(numbers, keypoint.Time)
			table.insert(numbers, keypoint.Value.R)
			table.insert(numbers, keypoint.Value.G)
			table.insert(numbers, keypoint.Value.B)
		end
		return numbers
	end
	return nil
end

local function colorChannelEq(a: number, b: number): boolean
	return numberEq(math.floor(a * 255), math.floor(b * 255))
end

local function trueEquals(a, b): boolean
	-- Exit early for simple equality values
	if a == b then
		return true
	end

	-- Treat nil and { Ref = "000...0" } as equal
	if
		(a == nil and type(b) == "table" and b.Ref == "00000000000000000000000000000000")
		or (b == nil and type(a) == "table" and a.Ref == "00000000000000000000000000000000")
	then
		return true
	end

	local typeA, typeB = typeof(a), typeof(b)

	-- For tables, try recursive deep equality
	if typeA == "table" and typeB == "table" then
		local checkedKeys = {}
		for key, value in a do
			checkedKeys[key] = true
			if not trueEquals(value, b[key]) then
				return false
			end
		end
		for key, value in b do
			if checkedKeys[key] then
				continue
			end
			if not trueEquals(value, a[key]) then
				return false
			end
		end
		return true

	-- A composite with a NaN component isn't equal to itself either, so NaN is
	-- only special-cased for plain numbers and inside components.
	elseif typeA == "number" and typeB == "number" then
		return numberEq(a, b)

	-- For EnumItem->number, compare the EnumItem's value
	elseif typeA == "number" and typeB == "EnumItem" then
		return a == b.Value
	elseif typeA == "EnumItem" and typeB == "number" then
		return a.Value == b

	-- For Color3s, compare to RGB ints to avoid floating point inequality
	elseif typeA == "Color3" and typeB == "Color3" then
		return colorChannelEq(a.R, b.R) and colorChannelEq(a.G, b.G) and colorChannelEq(a.B, b.B)
	end

	-- For other composites, compare each component as a number
	if typeA == typeB then
		local aComponents, bComponents = components(a), components(b)
		if aComponents ~= nil and bComponents ~= nil then
			if #aComponents ~= #bComponents then
				return false
			end
			for i, aComponent in aComponents do
				if not numberEq(aComponent, bComponents[i]) then
					return false
				end
			end
			return true
		end
	end

	return false
end

return trueEquals
