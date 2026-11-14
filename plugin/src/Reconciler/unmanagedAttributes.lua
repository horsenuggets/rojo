--[[
	Some attributes are never written to the filesystem, so their absence there
	says nothing about whether they should exist in Studio:

	> Internal attributes, prefixed with `RBX`, which syncback skips.
	> Rojo's own reference links, `Rojo_Id` and `Rojo_Target_*`, which the server
	  resolves into Ref properties instead of sending.
	> Composite values JSON can't hold, such as a Vector3 with a NaN component.
	  Plain numbers are always held, since NaN and infinity are written in a
	  tagged form.

	These are left exactly as they are in Studio when diffing and applying.
]]

local unmanagedAttributes = {}

local function isFinite(number)
	return number == number and number ~= math.huge and number ~= -math.huge
end

local function allFinite(...)
	for index = 1, select("#", ...) do
		if not isFinite(select(index, ...)) then
			return false
		end
	end
	return true
end

-- Mirrors the server's `is_json_compatible`, which decides what syncback skips.
local function isJsonCompatible(value)
	local valueType = typeof(value)
	if valueType == "Vector2" then
		return allFinite(value.X, value.Y)
	elseif valueType == "Vector3" then
		return allFinite(value.X, value.Y, value.Z)
	elseif valueType == "Color3" then
		return allFinite(value.R, value.G, value.B)
	elseif valueType == "CFrame" then
		return allFinite(value:GetComponents())
	elseif valueType == "NumberRange" then
		return allFinite(value.Min, value.Max)
	elseif valueType == "UDim" then
		return isFinite(value.Scale)
	elseif valueType == "UDim2" then
		return allFinite(value.X.Scale, value.Y.Scale)
	elseif valueType == "Rect" then
		return allFinite(value.Min.X, value.Min.Y, value.Max.X, value.Max.Y)
	elseif valueType == "NumberSequence" then
		for _, keypoint in value.Keypoints do
			if not allFinite(keypoint.Time, keypoint.Value, keypoint.Envelope) then
				return false
			end
		end
	elseif valueType == "ColorSequence" then
		for _, keypoint in value.Keypoints do
			if not allFinite(keypoint.Time, keypoint.Value.R, keypoint.Value.G, keypoint.Value.B) then
				return false
			end
		end
	end
	return true
end

function unmanagedAttributes.isUnmanaged(name, value)
	return string.sub(name, 1, 3) == "RBX"
		or name == "Rojo_Id"
		or string.sub(name, 1, 12) == "Rojo_Target_"
		or not isJsonCompatible(value)
end

-- Returns the attributes to give an instance: the managed values from
-- `incoming`, plus the unmanaged ones the instance already has.
function unmanagedAttributes.merge(incoming, existing)
	local merged = {}
	for name, value in incoming do
		if not unmanagedAttributes.isUnmanaged(name, value) then
			merged[name] = value
		end
	end
	for name, value in existing or {} do
		if unmanagedAttributes.isUnmanaged(name, value) then
			merged[name] = value
		end
	end
	return merged
end

return unmanagedAttributes
