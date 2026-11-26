--!nonstrict
--[[
	Derived from Roblox Roact 1.4.4, src/RobloxRenderer.lua, pinned at
	956891b70fdc5410e44e9664719cd0a9f7e6fabd. Licensed under Apache-2.0;
	see Packages/Roact/LICENSE.txt for the license distributed with this source.
	Modified for Rojo to track active refs and cancel superseded host work.
	The project maps this file into Roact so its sibling imports remain unchanged.
]]
--[[
	Renderer that deals in terms of Roblox Instances. This is the most
	well-supported renderer after NoopRenderer and is currently the only
	renderer that does anything.
]]

local Binding = require(script.Parent.Binding)
local Children = require(script.Parent.PropMarkers.Children)
local ElementKind = require(script.Parent.ElementKind)
local SingleEventManager = require(script.Parent.SingleEventManager)
local getDefaultInstanceProperty = require(script.Parent.getDefaultInstanceProperty)
local Ref = require(script.Parent.PropMarkers.Ref)
local Type = require(script.Parent.Type)
local internalAssert = require(script.Parent.internalAssert)

local config = require(script.Parent.GlobalConfig).get()

local applyPropsError = [[
Error applying props:
	%s
In element:
%s
]]

local updatePropsError = [[
Error updating props:
	%s
In element:
%s
]]

local function identity(...)
	return ...
end

local function beginHostUpdate(reconciler, virtualNode)
	virtualNode.hostUpdateCount = (virtualNode.hostUpdateCount or 0) + 1
	return {
		reconciler = reconciler,
		hostUpdateCount = virtualNode.hostUpdateCount,
		updateCount = virtualNode.updateCount,
	}
end

local function isHostUpdateCurrent(virtualNode, updateCount)
	return not virtualNode.hostUnmounted
		and virtualNode.hostUpdateCount == updateCount.hostUpdateCount
		and updateCount.reconciler.isUpdateCurrent(virtualNode, updateCount.updateCount)
end

local function applyRef(ref, newHostObject)
	if ref == nil then
		return
	end

	if typeof(ref) == "function" then
		ref(newHostObject)
	elseif Type.of(ref) == Type.Binding then
		Binding.update(ref, newHostObject)
	else
		-- TODO (#197): Better error message
		error(("Invalid ref: Expected type Binding but got %s"):format(typeof(ref)))
	end
end

local function detachRef(virtualNode)
	local ref = virtualNode.activeRef
	-- Publish the change before a callback can unmount or replace this ref again.
	virtualNode.activeRef = nil
	applyRef(ref, nil)
end

local function attachRef(virtualNode, ref)
	virtualNode.activeRef = ref
	applyRef(ref, virtualNode.hostObject)
end

local function setRobloxInstanceProperty(hostObject, key, newValue)
	if newValue == nil then
		local hostClass = hostObject.ClassName
		local _, defaultValue = getDefaultInstanceProperty(hostClass, key)
		newValue = defaultValue
	end

	-- Assign the new value to the object
	hostObject[key] = newValue

	return
end

local function removeBinding(virtualNode, key)
	local disconnect = virtualNode.bindings[key]
	virtualNode.bindings[key] = nil
	if virtualNode.pendingBindings ~= nil then
		virtualNode.pendingBindings[key] = nil
	end
	disconnect()
end

local function initializeBinding(virtualNode, key, binding, updateBoundProperty, updateCount)
	if virtualNode.pendingBindings == nil then
		virtualNode.pendingBindings = {}
	end
	-- Each owner gets its own initializer, but reuses the live subscription. An old
	-- getValue continuation must neither write nor clear a newer owner's pending read.
	local pending = { binding = binding, updateBoundProperty = updateBoundProperty }
	virtualNode.pendingBindings[key] = pending
	local value = binding:getValue()
	if isHostUpdateCurrent(virtualNode, updateCount) and virtualNode.pendingBindings[key] == pending then
		updateBoundProperty(value, true)
		if virtualNode.pendingBindings ~= nil and virtualNode.pendingBindings[key] == pending then
			virtualNode.pendingBindings[key] = nil
		end
	end
end

local function attachBinding(virtualNode, key, newBinding, updateCount)
	local disconnect
	local function updateBoundProperty(newValue, initializing)
		-- Unchanged bindings survive host updates; replaced subscriptions do not.
		if
			not updateCount.reconciler.isUpdateCurrent(virtualNode, virtualNode.updateCount)
			or virtualNode.hostUnmounted
			or virtualNode.bindings == nil
			or virtualNode.bindings[key] ~= disconnect
		then
			return
		end
		-- An emission supersedes initial reads. Invalidate them before property callbacks
		-- can reenter and publish a newer pending read.
		if not initializing and virtualNode.pendingBindings ~= nil then
			virtualNode.pendingBindings[key] = nil
		end
		local success, errorMessage = xpcall(function()
			setRobloxInstanceProperty(virtualNode.hostObject, key, newValue)
		end, identity)

		if not success then
			local source = virtualNode.currentElement.source

			if source == nil then
				source = "<enable element tracebacks>"
			end

			local fullMessage = updatePropsError:format(errorMessage, source)
			error(fullMessage, 0)
		end
	end

	if virtualNode.bindings == nil then
		virtualNode.bindings = {}
	end

	disconnect = Binding.subscribe(newBinding, updateBoundProperty)
	virtualNode.bindings[key] = disconnect

	initializeBinding(virtualNode, key, newBinding, updateBoundProperty, updateCount)
end

local function detachAllBindings(virtualNode)
	virtualNode.pendingBindings = nil
	if virtualNode.bindings ~= nil then
		local bindings = virtualNode.bindings
		virtualNode.bindings = nil
		for _, disconnect in pairs(bindings) do
			disconnect()
		end
	end
end

local function applyProp(virtualNode, key, newValue, oldValue, updateCount)
	if not isHostUpdateCurrent(virtualNode, updateCount) then
		return
	end
	if newValue == oldValue then
		local pending = virtualNode.pendingBindings and virtualNode.pendingBindings[key]
		if pending ~= nil then
			initializeBinding(virtualNode, key, pending.binding, pending.updateBoundProperty, updateCount)
		end
		return
	end

	if key == Ref or key == Children then
		-- Refs and children are handled in a separate pass
		return
	end

	-- Track the applied props, not an element that may still be rendering. Reentry
	-- must diff against subscriptions and writes already owned by this host.
	virtualNode.hostProps[key] = newValue
	local internalKeyType = Type.of(key)

	if internalKeyType == Type.HostEvent or internalKeyType == Type.HostChangeEvent then
		if virtualNode.eventManager == nil then
			virtualNode.eventManager = SingleEventManager.new(virtualNode.hostObject)
		end

		local eventName = key.name

		if internalKeyType == Type.HostChangeEvent then
			virtualNode.eventManager:connectPropertyChange(eventName, newValue)
		else
			virtualNode.eventManager:connectEvent(eventName, newValue)
		end

		return
	end

	local newIsBinding = Type.of(newValue) == Type.Binding
	local oldIsBinding = Type.of(oldValue) == Type.Binding

	if oldIsBinding then
		removeBinding(virtualNode, key)
	end

	if newIsBinding then
		attachBinding(virtualNode, key, newValue, updateCount)
	else
		setRobloxInstanceProperty(virtualNode.hostObject, key, newValue)
	end
end

local function applyProps(virtualNode, props, updateCount)
	for propKey, value in pairs(props) do
		if not isHostUpdateCurrent(virtualNode, updateCount) then
			return
		end
		applyProp(virtualNode, propKey, value, nil, updateCount)
	end
end

local function updateProps(virtualNode, oldProps, newProps, updateCount)
	-- Apply props that were added or updated
	for propKey, newValue in pairs(newProps) do
		if not isHostUpdateCurrent(virtualNode, updateCount) then
			return
		end
		applyProp(virtualNode, propKey, newValue, oldProps[propKey], updateCount)
	end

	-- Remove props that were removed
	for propKey, oldValue in pairs(oldProps) do
		if not isHostUpdateCurrent(virtualNode, updateCount) then
			return
		end
		if newProps[propKey] == nil then
			applyProp(virtualNode, propKey, nil, oldValue, updateCount)
		end
	end
end

local RobloxRenderer = {}

function RobloxRenderer.isHostObject(target)
	return typeof(target) == "Instance"
end

function RobloxRenderer.mountHostNode(reconciler, virtualNode)
	local element = virtualNode.currentElement
	local hostParent = virtualNode.hostParent
	local hostKey = virtualNode.hostKey

	if config.internalTypeChecks then
		internalAssert(ElementKind.of(element) == ElementKind.Host, "Element at given node is not a host Element")
	end
	if config.typeChecks then
		assert(element.props.Name == nil, "Name can not be specified as a prop to a host component in Roact.")
		assert(element.props.Parent == nil, "Parent can not be specified as a prop to a host component in Roact.")
	end

	local updateCount = beginHostUpdate(reconciler, virtualNode)
	local instance = Instance.new(element.component)
	virtualNode.hostObject = instance
	virtualNode.hostProps = {}

	local success, errorMessage = xpcall(function()
		applyProps(virtualNode, element.props, updateCount)
	end, identity)

	if not success then
		local source = element.source

		if source == nil then
			source = "<enable element tracebacks>"
		end

		local fullMessage = applyPropsError:format(errorMessage, source)
		error(fullMessage, 0)
	end
	if not isHostUpdateCurrent(virtualNode, updateCount) then
		return
	end

	local name = tostring(hostKey)
	if not isHostUpdateCurrent(virtualNode, updateCount) then
		return
	end
	instance.Name = name

	local children = element.props[Children]

	if children ~= nil then
		reconciler.updateVirtualNodeWithChildren(virtualNode, virtualNode.hostObject, children)
		if not isHostUpdateCurrent(virtualNode, updateCount) then
			return
		end
	end

	instance.Parent = hostParent
	if not isHostUpdateCurrent(virtualNode, updateCount) then
		return
	end

	attachRef(virtualNode, element.props[Ref])
	if not isHostUpdateCurrent(virtualNode, updateCount) then
		return
	end

	if virtualNode.eventManager ~= nil then
		virtualNode.eventManager:resume()
	end
end

function RobloxRenderer.unmountHostNode(reconciler, virtualNode)
	if virtualNode.hostUnmounted then
		return
	end
	virtualNode.hostUnmounted = true
	beginHostUpdate(reconciler, virtualNode)
	if virtualNode.eventManager ~= nil then
		virtualNode.eventManager:suspend()
	end
	detachAllBindings(virtualNode)
	detachRef(virtualNode)

	for _, childNode in pairs(virtualNode.children) do
		reconciler.unmountVirtualNode(childNode)
	end

	virtualNode.hostObject:Destroy()
end

function RobloxRenderer.updateHostNode(reconciler, virtualNode, newElement)
	if virtualNode.wasUnmounted or virtualNode.hostUnmounted then
		return virtualNode
	end
	local updateCount = beginHostUpdate(reconciler, virtualNode)
	local oldProps = virtualNode.currentElement.props
	local newProps = newElement.props

	if virtualNode.eventManager ~= nil then
		virtualNode.eventManager:suspend()
	end

	-- The active ref can differ from currentElement during a callback or yield.
	if virtualNode.activeRef ~= newProps[Ref] then
		detachRef(virtualNode)
		if not isHostUpdateCurrent(virtualNode, updateCount) then
			return virtualNode
		end
		attachRef(virtualNode, newProps[Ref])
		if not isHostUpdateCurrent(virtualNode, updateCount) then
			return virtualNode
		end
	end

	local success, errorMessage = xpcall(function()
		updateProps(virtualNode, virtualNode.hostProps, newProps, updateCount)
	end, identity)

	if not success then
		local source = newElement.source

		if source == nil then
			source = "<enable element tracebacks>"
		end

		local fullMessage = updatePropsError:format(errorMessage, source)
		error(fullMessage, 0)
	end
	if not isHostUpdateCurrent(virtualNode, updateCount) then
		return virtualNode
	end

	local children = newElement.props[Children]
	if children ~= nil or oldProps[Children] ~= nil or next(virtualNode.children) ~= nil then
		reconciler.updateVirtualNodeWithChildren(virtualNode, virtualNode.hostObject, children)
		if not isHostUpdateCurrent(virtualNode, updateCount) then
			return virtualNode
		end
	end

	if virtualNode.eventManager ~= nil then
		virtualNode.eventManager:resume()
	end

	return virtualNode
end

return RobloxRenderer
