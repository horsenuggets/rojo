--!nonstrict
--[[
	Derived from Roblox Roact 1.4.4, src/Component.lua, pinned at
	956891b70fdc5410e44e9664719cd0a9f7e6fabd. Licensed under Apache-2.0;
	see Packages/Roact/LICENSE.txt for the license distributed with this source.
	Modified for Rojo to cancel retired or superseded lifecycle work.
	The project maps this file into Roact so its sibling imports remain unchanged.
]]
local assign = require(script.Parent.assign)
local ComponentLifecyclePhase = require(script.Parent.ComponentLifecyclePhase)
local Type = require(script.Parent.Type)
local Symbol = require(script.Parent.Symbol)
local invalidSetStateMessages = require(script.Parent.invalidSetStateMessages)
local internalAssert = require(script.Parent.internalAssert)

local config = require(script.Parent.GlobalConfig).get()

--[[
	Calling setState during certain lifecycle allowed methods has the potential
	to create an infinitely updating component. Rather than time out, we exit
	with an error if an unreasonable number of self-triggering updates occur
]]
local MAX_PENDING_UPDATES = 100

local InternalData = Symbol.named("InternalData")

local componentMissingRenderMessage = [[
The component %q is missing the `render` method.
`render` must be defined when creating a Roact component!]]

local tooManyUpdatesMessage = [[
The component %q has reached the setState update recursion limit.
When using `setState` in `didUpdate`, make sure that it won't repeat infinitely!]]

local function isUpdateCurrent(internalData, updateCount)
	return not internalData.retired and internalData.reconciler.isUpdateCurrent(internalData.virtualNode, updateCount)
end

local componentClassMetatable = {}

function componentClassMetatable:__tostring()
	return self.__componentName
end

local Component = {}
setmetatable(Component, componentClassMetatable)

Component[Type] = Type.StatefulComponentClass
Component.__index = Component
Component.__componentName = "Component"

--[[
	A method called by consumers of Roact to create a new component class.
	Components can not be extended beyond this point, with the exception of
	PureComponent.
]]
function Component:extend(name)
	if config.typeChecks then
		assert(Type.of(self) == Type.StatefulComponentClass, "Invalid `self` argument to `extend`.")
		assert(typeof(name) == "string", "Component class name must be a string")
	end

	local class = {}

	for key, value in pairs(self) do
		-- Roact opts to make consumers use composition over inheritance, which
		-- lines up with React.
		-- https://reactjs.org/docs/composition-vs-inheritance.html
		if key ~= "extend" then
			class[key] = value
		end
	end

	class[Type] = Type.StatefulComponentClass
	class.__index = class
	class.__componentName = name

	setmetatable(class, componentClassMetatable)

	return class
end

function Component:__getDerivedState(incomingProps, incomingState)
	if config.internalTypeChecks then
		internalAssert(Type.of(self) == Type.StatefulComponentInstance, "Invalid use of `__getDerivedState`")
	end

	local internalData = self[InternalData]
	local componentClass = internalData.componentClass

	if componentClass.getDerivedStateFromProps ~= nil then
		local derivedState = componentClass.getDerivedStateFromProps(incomingProps, incomingState)

		if derivedState ~= nil then
			if config.typeChecks then
				assert(typeof(derivedState) == "table", "getDerivedStateFromProps must return a table!")
			end

			return derivedState
		end
	end

	return nil
end

function Component:setState(mapState)
	if config.typeChecks then
		assert(Type.of(self) == Type.StatefulComponentInstance, "Invalid `self` argument to `extend`.")
	end

	local internalData = self[InternalData]
	local updateCount = internalData.virtualNode.updateCount
	if not isUpdateCurrent(internalData, updateCount) then
		return
	end
	local lifecyclePhase = internalData.lifecyclePhase

	--[[
		When preparing to update, render, or unmount, it is not safe
		to call `setState` as it will interfere with in-flight updates. It's
		also disallowed during unmounting
	]]
	if
		lifecyclePhase == ComponentLifecyclePhase.ShouldUpdate
		or lifecyclePhase == ComponentLifecyclePhase.WillUpdate
		or lifecyclePhase == ComponentLifecyclePhase.Render
	then
		local messageTemplate = invalidSetStateMessages[internalData.lifecyclePhase]

		local message = messageTemplate:format(tostring(internalData.componentClass))
		error(message, 2)
	elseif lifecyclePhase == ComponentLifecyclePhase.WillUnmount then
		-- Should not print error message. See https://github.com/facebook/react/pull/22114
		return
	end

	local pendingState = internalData.pendingState

	local partialState
	if typeof(mapState) == "function" then
		partialState = mapState(pendingState or self.state, self.props)

		-- Abort the state update if the given state updater function returns nil
		if partialState == nil then
			return
		end
	elseif typeof(mapState) == "table" then
		partialState = mapState
	else
		error("Invalid argument to setState, expected function or table", 2)
	end

	if not isUpdateCurrent(internalData, updateCount) then
		return
	end

	local newState
	if pendingState ~= nil then
		newState = assign(pendingState, partialState)
	else
		newState = assign({}, self.state, partialState)
	end

	if lifecyclePhase == ComponentLifecyclePhase.Init then
		-- If `setState` is called in `init`, we can skip triggering an update!
		local derivedState = self:__getDerivedState(self.props, newState)
		if not isUpdateCurrent(internalData, updateCount) then
			return
		end
		self.state = assign(newState, derivedState)
	elseif
		lifecyclePhase == ComponentLifecyclePhase.DidMount
		or lifecyclePhase == ComponentLifecyclePhase.DidUpdate
		or lifecyclePhase == ComponentLifecyclePhase.ReconcileChildren
	then
		--[[
			During certain phases of the component lifecycle, it's acceptable to
			allow `setState` but defer the update until we're done with ones in flight.
			We do this by collapsing it into any pending updates we have.
		]]
		local derivedState = self:__getDerivedState(self.props, newState)
		if not isUpdateCurrent(internalData, updateCount) then
			return
		end
		internalData.pendingState = assign(newState, derivedState)
	elseif lifecyclePhase == ComponentLifecyclePhase.Idle then
		-- Outside of our lifecycle, the state update is safe to make immediately
		self:__update(nil, newState)
	else
		local messageTemplate = invalidSetStateMessages.default

		local message = messageTemplate:format(tostring(internalData.componentClass))

		error(message, 2)
	end
end

--[[
	Returns the stack trace of where the element was created that this component
	instance's properties are based on.

	Intended to be used primarily by diagnostic tools.
]]
function Component:getElementTraceback()
	return self[InternalData].virtualNode.currentElement.source
end

--[[
	Returns a snapshot of this component given the current props and state. Must
	be overridden by consumers of Roact and should be a pure function with
	regards to props and state.

	TODO (#199): Accept props and state as arguments.
]]
function Component:render()
	local internalData = self[InternalData]

	local message = componentMissingRenderMessage:format(tostring(internalData.componentClass))

	error(message, 0)
end

--[[
	Retrieves the context value corresponding to the given key. Can return nil
	if a requested context key is not present
]]
function Component:__getContext(key)
	if config.internalTypeChecks then
		internalAssert(Type.of(self) == Type.StatefulComponentInstance, "Invalid use of `__getContext`")
		internalAssert(key ~= nil, "Context key cannot be nil")
	end

	local virtualNode = self[InternalData].virtualNode
	local context = virtualNode.context

	return context[key]
end

--[[
	Adds a new context entry to this component's context table (which will be
	passed down to child components).
]]
function Component:__addContext(key, value)
	if config.internalTypeChecks then
		internalAssert(Type.of(self) == Type.StatefulComponentInstance, "Invalid use of `__addContext`")
	end
	local virtualNode = self[InternalData].virtualNode

	-- Make sure we store a reference to the component's original, unmodified
	-- context the virtual node. In the reconciler, we'll restore the original
	-- context if we need to replace the node (this happens when a node gets
	-- re-rendered as a different component)
	if virtualNode.originalContext == nil then
		virtualNode.originalContext = virtualNode.context
	end

	-- Build a new context table on top of the existing one, then apply it to
	-- our virtualNode
	local existing = virtualNode.context
	virtualNode.context = assign({}, existing, { [key] = value })
end

--[[
	Performs property validation if the static method validateProps is declared.
	validateProps should follow assert's expected arguments:
	(false, message: string) | true. The function may return a message in the
	true case; it will be ignored. If this fails, the function will throw the
	error.
]]
function Component:__validateProps(props)
	if not config.propValidation then
		return
	end

	local validator = self[InternalData].componentClass.validateProps

	if validator == nil then
		return
	end

	if typeof(validator) ~= "function" then
		error(
			("validateProps must be a function, but it is a %s.\nCheck the definition of the component %q."):format(
				typeof(validator),
				self.__componentName
			)
		)
	end

	local success, failureReason = validator(props)

	if not success then
		failureReason = failureReason or "<Validator function did not supply a message>"
		error(
			("Property validation failed in %s: %s\n\n%s"):format(
				self.__componentName,
				tostring(failureReason),
				self:getElementTraceback() or "<enable element tracebacks>"
			),
			0
		)
	end
end

--[[
	An internal method used by the reconciler to construct a new component
	instance and attach it to the given virtualNode.
]]
function Component:__mount(reconciler, virtualNode)
	if config.internalTypeChecks then
		internalAssert(Type.of(self) == Type.StatefulComponentClass, "Invalid use of `__mount`")
		internalAssert(Type.of(virtualNode) == Type.VirtualNode, "Expected arg #2 to be of type VirtualNode")
	end

	local currentElement = virtualNode.currentElement
	local hostParent = virtualNode.hostParent
	local updateCount = virtualNode.updateCount

	-- Contains all the information that we want to keep from consumers of
	-- Roact, or even other parts of the codebase like the reconciler.
	local internalData = {
		reconciler = reconciler,
		virtualNode = virtualNode,
		componentClass = self,
		lifecyclePhase = ComponentLifecyclePhase.Init,
		pendingState = nil,
		retired = false,
	}

	local instance = {
		[Type] = Type.StatefulComponentInstance,
		[InternalData] = internalData,
	}

	setmetatable(instance, self)

	virtualNode.instance = instance

	local props = currentElement.props

	if self.defaultProps ~= nil then
		props = assign({}, self.defaultProps, props)
	end

	instance:__validateProps(props)
	if not isUpdateCurrent(internalData, updateCount) then
		return
	end

	instance.props = props

	local newContext = assign({}, virtualNode.legacyContext)
	instance._context = newContext

	local derivedState = instance:__getDerivedState(instance.props, {})
	if not isUpdateCurrent(internalData, updateCount) then
		return
	end
	instance.state = assign({}, derivedState)

	if instance.init ~= nil then
		instance:init(instance.props)
		if not isUpdateCurrent(internalData, updateCount) then
			return
		end
		derivedState = instance:__getDerivedState(instance.props, instance.state)
		if not isUpdateCurrent(internalData, updateCount) then
			return
		end
		assign(instance.state, derivedState)
	end

	-- It's possible for init() to redefine _context!
	virtualNode.legacyContext = instance._context

	internalData.lifecyclePhase = ComponentLifecyclePhase.Render
	local renderResult = instance:render()
	if not isUpdateCurrent(internalData, updateCount) then
		return
	end

	internalData.lifecyclePhase = ComponentLifecyclePhase.ReconcileChildren
	reconciler.updateVirtualNodeWithRenderResult(virtualNode, hostParent, renderResult)
	if not isUpdateCurrent(internalData, updateCount) then
		return
	end

	if instance.didMount ~= nil then
		internalData.lifecyclePhase = ComponentLifecyclePhase.DidMount
		instance:didMount()
		if not isUpdateCurrent(internalData, updateCount) then
			return
		end
	end

	if internalData.pendingState ~= nil then
		-- __update will handle pendingState, including its final lifecycle phase.
		instance:__update(nil, nil)
		return
	end

	internalData.lifecyclePhase = ComponentLifecyclePhase.Idle
end

--[[
	Internal method used by the reconciler to clean up any resources held by
	this component instance.
]]
function Component:__unmount()
	if config.internalTypeChecks then
		internalAssert(Type.of(self) == Type.StatefulComponentInstance, "Invalid use of `__unmount`")
	end

	local internalData = self[InternalData]
	local virtualNode = internalData.virtualNode
	local reconciler = internalData.reconciler

	if internalData.retired then
		return
	end
	internalData.retired = true
	internalData.pendingState = nil
	-- Retirement is permanent, including components without a willUnmount callback.
	internalData.lifecyclePhase = ComponentLifecyclePhase.WillUnmount
	if self.willUnmount ~= nil then
		self:willUnmount()
	end

	for _, childNode in pairs(virtualNode.children) do
		reconciler.unmountVirtualNode(childNode)
	end
end

--[[
	Internal method used by setState (to trigger updates based on state) and by
	the reconciler (to trigger updates based on props)

	Returns true if the update was completed, false if it was cancelled.
]]
function Component:__update(updatedElement, updatedState, updateCount)
	if config.internalTypeChecks then
		internalAssert(Type.of(self) == Type.StatefulComponentInstance, "Invalid use of `__update`")
		internalAssert(
			Type.of(updatedElement) == Type.Element or updatedElement == nil,
			"Expected arg #1 to be of type Element or nil"
		)
		internalAssert(
			typeof(updatedState) == "table" or updatedState == nil,
			"Expected arg #2 to be of type table or nil"
		)
	end

	local internalData = self[InternalData]
	local virtualNode = internalData.virtualNode
	local componentClass = internalData.componentClass
	if internalData.retired or virtualNode.wasUnmounted then
		return false
	end
	-- State-only updates bypass updateVirtualNode but must own the same lifetime.
	if updateCount == nil then
		updateCount = internalData.reconciler.beginUpdate(virtualNode)
	end
	if not isUpdateCurrent(internalData, updateCount) then
		return false
	end

	local newProps = self.props
	if updatedElement ~= nil then
		newProps = updatedElement.props

		if componentClass.defaultProps ~= nil then
			newProps = assign({}, componentClass.defaultProps, newProps)
		end

		self:__validateProps(newProps)
		if not isUpdateCurrent(internalData, updateCount) then
			return false
		end
	end

	local updateCountForLimit = 0
	repeat
		local finalState
		local pendingState = nil

		-- Consume any pending state we might have
		if internalData.pendingState ~= nil then
			pendingState = internalData.pendingState
			internalData.pendingState = nil
		end

		-- Consume a standard update to state or props
		if updatedState ~= nil or newProps ~= self.props then
			if pendingState == nil then
				finalState = updatedState or self.state
			else
				finalState = assign(pendingState, updatedState)
			end

			local derivedState = self:__getDerivedState(newProps, finalState)
			if not isUpdateCurrent(internalData, updateCount) then
				return false
			end

			if derivedState ~= nil then
				finalState = assign({}, finalState, derivedState)
			end

			updatedState = nil
		else
			finalState = pendingState
		end

		if not self:__resolveUpdate(newProps, finalState, updateCount) then
			-- Only the owning update can finish the active pass, including shouldUpdate.
			if isUpdateCurrent(internalData, updateCount) then
				virtualNode.activeUpdate = nil
			end
			return false
		end

		updateCountForLimit += 1

		if updateCountForLimit > MAX_PENDING_UPDATES then
			error(tooManyUpdatesMessage:format(tostring(internalData.componentClass)), 3)
		end
	until internalData.pendingState == nil

	virtualNode.activeUpdate = nil
	return true
end

--[[
	Internal method used by __update to apply new props and state

	Returns true if the update was completed, false if it was cancelled.
]]
function Component:__resolveUpdate(incomingProps, incomingState, updateCount)
	if config.internalTypeChecks then
		internalAssert(Type.of(self) == Type.StatefulComponentInstance, "Invalid use of `__resolveUpdate`")
	end

	local internalData = self[InternalData]
	local virtualNode = internalData.virtualNode
	local reconciler = internalData.reconciler

	if not isUpdateCurrent(internalData, updateCount) then
		return false
	end

	local oldProps = self.props
	local oldState = self.state

	if incomingProps == nil then
		incomingProps = oldProps
	end
	if incomingState == nil then
		incomingState = oldState
	end

	if self.shouldUpdate ~= nil then
		internalData.lifecyclePhase = ComponentLifecyclePhase.ShouldUpdate
		local continueWithUpdate = self:shouldUpdate(incomingProps, incomingState)
		if not isUpdateCurrent(internalData, updateCount) then
			return false
		end

		if not continueWithUpdate then
			internalData.lifecyclePhase = ComponentLifecyclePhase.Idle
			return false
		end
	end

	if self.willUpdate ~= nil then
		internalData.lifecyclePhase = ComponentLifecyclePhase.WillUpdate
		self:willUpdate(incomingProps, incomingState)
		if not isUpdateCurrent(internalData, updateCount) then
			return false
		end
	end

	internalData.lifecyclePhase = ComponentLifecyclePhase.Render

	self.props = incomingProps
	self.state = incomingState

	local renderResult = virtualNode.instance:render()
	if not isUpdateCurrent(internalData, updateCount) then
		return false
	end

	internalData.lifecyclePhase = ComponentLifecyclePhase.ReconcileChildren
	reconciler.updateVirtualNodeWithRenderResult(virtualNode, virtualNode.hostParent, renderResult)
	if not isUpdateCurrent(internalData, updateCount) then
		return false
	end

	if self.didUpdate ~= nil then
		internalData.lifecyclePhase = ComponentLifecyclePhase.DidUpdate
		self:didUpdate(oldProps, oldState)
		if not isUpdateCurrent(internalData, updateCount) then
			return false
		end
	end

	internalData.lifecyclePhase = ComponentLifecyclePhase.Idle
	return true
end

return Component
