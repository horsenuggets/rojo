--[[
	Services are owned by the engine rather than the place. Studio creates many
	of them lazily, so a project can describe a service that hasn't been made yet,
	and the DataModel always holds services that no project describes.
]]

local Packages = script.Parent.Parent.Parent.Packages
local ReflectionDatabase = require(Packages.RbxDom.database)

local services = {}

local function hasTag(className, tag)
	local class = ReflectionDatabase.Classes[className]
	return class ~= nil and class.Tags ~= nil and table.find(class.Tags, tag) ~= nil
end

local function isDataModel(instance)
	local success, result = pcall(function()
		return instance:IsA("DataModel")
	end)
	return success and result
end

-- Returns whether the engine owns `instance`, so Rojo must never remove it even
-- when the project doesn't describe it. Everything directly under a DataModel
-- is engine-owned, and so are instances of classes that can't be created, since
-- the engine makes those itself (like TouchInterest) and syncback only writes
-- them when the place actually contains them.
function services.isEngineOwned(instance)
	local success, result = pcall(function()
		local parent = instance.Parent
		return (parent ~= nil and isDataModel(parent)) or hasTag(instance.ClassName, "NotCreatable")
	end)
	return success and result
end

-- Returns the service of `className` under `parent`, creating it when Studio
-- hasn't yet. Returns nil if `parent` isn't a DataModel or the class isn't a
-- service. Creating a lazy service matches what applying the patch would do,
-- and lets its engine-made children be matched instead of shown as additions.
function services.getService(parent, className)
	if not isDataModel(parent) then
		return nil
	end
	local success, service = pcall(parent.GetService, parent, className)
	if success and service ~= nil and service.Parent == parent then
		return service
	end
	return nil
end

return services
