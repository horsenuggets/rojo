--[[
	Attempts to set a property on the given instance.
]]

local Packages = script.Parent.Parent.Parent.Packages
local Log = require(Packages.Log)
local RbxDom = require(Packages.RbxDom)
local Error = require(script.Parent.Error)
local createSourceSetter = require(script.Parent.setSource)

local ScriptEditorService = game:GetService("ScriptEditorService")
local sourceObservers = {}
local sourceObserverCount = 0
local documentChanges
local setSource = createSourceSetter({
	ConnectChanged = function(instance, changed)
		sourceObservers[instance] = changed
		sourceObserverCount += 1
		if documentChanges == nil then
			-- Dispatch once per document change, not once per pending initial-sync script.
			documentChanges = ScriptEditorService.TextDocumentDidChange:Connect(function(document)
				local observer = sourceObservers[document:GetScript()]
				if observer then
					observer()
				end
			end)
		end
		local sourceChanges = instance:GetPropertyChangedSignal("Source"):Connect(function()
			-- Open documents already report their edits through TextDocumentDidChange.
			if ScriptEditorService:FindScriptDocument(instance) == nil then
				changed()
			end
		end)
		return {
			Disconnect = function()
				sourceChanges:Disconnect()
				if sourceObservers[instance] == changed then
					sourceObservers[instance] = nil
					sourceObserverCount -= 1
					if sourceObserverCount == 0 then
						documentChanges:Disconnect()
						documentChanges = nil
					end
				end
			end,
		}
	end,
	GetEditorSource = function(instance)
		return ScriptEditorService:GetEditorSource(instance)
	end,
	Spawn = task.spawn,
	UpdateSourceAsync = function(instance, callback)
		ScriptEditorService:UpdateSourceAsync(instance, callback)
	end,
})

local function setProperty(instance, propertyName, value)
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

	local writeSuccess, err
	if propertyName == "Source" and instance:IsA("LuaSourceContainer") then
		writeSuccess, err = pcall(setSource, instance, value)
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

	return true
end

return setProperty
