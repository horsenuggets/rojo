--[[
	Attempts to set a property on the given instance.
]]

local Packages = script.Parent.Parent.Parent.Packages
local Log = require(Packages.Log)
local RbxDom = require(Packages.RbxDom)
local Error = require(script.Parent.Error)
local createSourceSetter = require(script.Parent.setSource)

local setSource = createSourceSetter.shared()

-- Source success means accepted, not committed. The third result is an operation whose
-- terminal outcome is also reported through the map's onSourceOutcome callback.
local function setProperty(instance, propertyName, value, instanceMap, serverOperation)
	local descriptor = RbxDom.findCanonicalPropertyDescriptor(instance.ClassName, propertyName)

	-- We can skip unknown properties; they're not likely reflected to Lua.
	--
	-- A good example of a property like this is `Model.ModelInPrimary`, which
	-- is serialized but not reflected to Lua.
	if descriptor == nil then
		Log.trace("Skipping unknown property {}.{}", instance.ClassName, propertyName)

		return true
	end

	if descriptor.scriptability == "None" or descriptor.scriptability == "Read" then
		return false,
			Error.new(Error.UnwritableProperty, {
				className = instance.ClassName,
				propertyName = propertyName,
			})
	end

	local writeSuccess, err, operation
	if propertyName == "Source" and instance:IsA("LuaSourceContainer") then
		if instanceMap then
			writeSuccess, err = pcall(instanceMap.setSource, instanceMap, instance, value, nil, serverOperation)
		else
			writeSuccess, err = pcall(setSource, instance, value, nil, serverOperation)
		end
		if writeSuccess then
			operation = err
			if operation.Status == "failed" then
				writeSuccess, err = false, operation.Error
			end
		end
		if not writeSuccess then
			err = RbxDom.Error.new(RbxDom.Error.Kind.Roblox, tostring(err))
		end
	else
		writeSuccess, err = descriptor:write(instance, value)
	end

	if not writeSuccess then
		if err.kind == RbxDom.Error.Kind.Roblox and err.extra:find("lacking permission") then
			return false,
				Error.new(Error.LackingPropertyPermissions, {
					className = instance.ClassName,
					propertyName = propertyName,
				})
		end

		return false,
			Error.new(Error.OtherPropertyError, {
				className = instance.ClassName,
				propertyName = propertyName,
			})
	end

	return true, nil, operation
end

return setProperty
