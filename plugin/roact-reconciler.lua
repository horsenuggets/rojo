--!nonstrict
--[[
	Derived from Roblox Roact 1.4.4, src/createReconciler.lua, pinned at
	956891b70fdc5410e44e9664719cd0a9f7e6fabd. Licensed under Apache-2.0;
	see Packages/Roact/LICENSE.txt for the license distributed with this source.
	Modified for Rojo to retire unmounted nodes and discard stale reconciliation work.
	The project maps this file into Roact so its sibling imports remain unchanged.
]]
local Type = require(script.Parent.Type)
local ElementKind = require(script.Parent.ElementKind)
local ElementUtils = require(script.Parent.ElementUtils)
local Children = require(script.Parent.PropMarkers.Children)
local Symbol = require(script.Parent.Symbol)
local internalAssert = require(script.Parent.internalAssert)

local config = require(script.Parent.GlobalConfig).get()

local InternalData = Symbol.named("InternalData")

--[[
	The reconciler is the mechanism in Roact that constructs the virtual tree
	that later gets turned into concrete objects by the renderer.

	Roact's reconciler is constructed with the renderer as an argument, which
	enables switching to different renderers for different platforms or
	scenarios.

	When testing the reconciler itself, it's common to use `NoopRenderer` with
	spies replacing some methods. The default (and only) reconciler interface
	exposed by Roact right now uses `RobloxRenderer`.
]]
local function createReconciler(renderer)
	local reconciler
	local mountVirtualNode
	local updateVirtualNode
	local unmountVirtualNode

	local function beginUpdate(virtualNode)
		virtualNode.updateCount += 1
		virtualNode.activeUpdate = virtualNode.updateCount
		-- A newer render also supersedes child work that has not finished yet.
		virtualNode.updateChildrenCount += 1
		return virtualNode.updateCount
	end

	local function isUpdateCurrent(virtualNode, updateCount)
		if virtualNode.updateCount ~= updateCount then
			return false
		end
		-- Unpublished mounts still belong to their parent pass or root update.
		local node = virtualNode
		while node do
			if node.wasUnmounted then
				return false
			end
			if
				node.mountParentUpdateChildrenCount ~= nil
				and node.parent.updateChildrenCount ~= node.mountParentUpdateChildrenCount
			then
				return false
			end
			if node.treeData then
				if not node.treeData.mounted then
					return false
				end
				if node.mountTreeUpdateCount ~= nil and node.treeData.updateCount ~= node.mountTreeUpdateCount then
					return false
				end
			end
			node = node.parent
		end
		return true
	end

	--[[
		Unmount the given virtualNode, replacing it with a new node described by
		the given element.

		Preserves host properties, depth, and legacyContext from parent.
	]]
	local function replaceVirtualNode(virtualNode, newElement)
		local hostParent = virtualNode.hostParent
		local hostKey = virtualNode.hostKey
		local depth = virtualNode.depth
		local parent = virtualNode.parent
		local parentUpdateChildrenCount = parent and parent.updateChildrenCount
		local treeData = virtualNode.treeData
		local treeUpdateCount = treeData and treeData.updateCount

		-- If the node that is being replaced has modified context, we need to
		-- use the original *unmodified* context for the new node
		-- The `originalContext` field will be nil if the context was unchanged
		local context = virtualNode.originalContext or virtualNode.context
		local parentLegacyContext = virtualNode.parentLegacyContext

		-- If updating this node has caused a component higher up the tree to re-render
		-- and updateChildren to be re-entered then this node could already have been
		-- unmounted in the previous updateChildren pass.
		if not virtualNode.wasUnmounted then
			unmountVirtualNode(virtualNode)
		end
		-- Unmount callbacks can remove the parent or give a newer child pass ownership.
		if
			(parent and parent.updateChildrenCount ~= parentUpdateChildrenCount)
			or (treeData and (not treeData.mounted or treeData.updateCount ~= treeUpdateCount))
		then
			return nil
		end

		local newNode =
			mountVirtualNode(newElement, hostParent, hostKey, context, parentLegacyContext, parent, treeData)

		if parent and parent.updateChildrenCount ~= parentUpdateChildrenCount then
			if newNode then
				unmountVirtualNode(newNode)
			end
			return nil
		end

		-- mountVirtualNode can return nil if the element is a boolean
		if newNode ~= nil then
			newNode.depth = depth
			newNode.parent = parent
		end

		return newNode
	end

	--[[
		Utility to update the children of a virtual node based on zero or more
		updated children given as elements.
	]]
	local function updateChildren(virtualNode, hostParent, newChildElements)
		if config.internalTypeChecks then
			internalAssert(Type.of(virtualNode) == Type.VirtualNode, "Expected arg #1 to be of type VirtualNode")
		end

		if not isUpdateCurrent(virtualNode, virtualNode.updateCount) then
			return
		end

		virtualNode.updateChildrenCount = virtualNode.updateChildrenCount + 1

		local currentUpdateChildrenCount = virtualNode.updateChildrenCount

		local removeKeys = {}

		-- Changed or removed children
		for childKey, childNode in pairs(virtualNode.children) do
			local newElement = ElementUtils.getElementByKey(newChildElements, childKey)
			local newNode = updateVirtualNode(childNode, newElement)

			-- If updating this node has caused a component higher up the tree to re-render
			-- and updateChildren to be re-entered for this virtualNode then
			-- this result is invalid and needs to be disgarded.
			if virtualNode.updateChildrenCount ~= currentUpdateChildrenCount then
				if newNode and newNode ~= virtualNode.children[childKey] then
					unmountVirtualNode(newNode)
				end
				return
			end

			if newNode ~= nil then
				virtualNode.children[childKey] = newNode
			else
				removeKeys[childKey] = true
			end
		end

		for childKey in pairs(removeKeys) do
			virtualNode.children[childKey] = nil
		end

		-- Added children
		for childKey, newElement in ElementUtils.iterateElements(newChildElements) do
			local concreteKey = childKey
			if childKey == ElementUtils.UseParentKey then
				concreteKey = virtualNode.hostKey
			end

			if virtualNode.children[childKey] == nil then
				local childNode = mountVirtualNode(
					newElement,
					hostParent,
					concreteKey,
					virtualNode.context,
					virtualNode.legacyContext,
					virtualNode
				)

				-- If updating this node has caused a component higher up the tree to re-render
				-- and updateChildren to be re-entered for this virtualNode then
				-- this result is invalid and needs to be discarded.
				if virtualNode.updateChildrenCount ~= currentUpdateChildrenCount then
					if childNode then
						unmountVirtualNode(childNode)
					end
					return
				end

				-- mountVirtualNode can return nil if the element is a boolean
				if childNode ~= nil then
					childNode.depth = virtualNode.depth + 1
					childNode.parent = virtualNode
					virtualNode.children[childKey] = childNode
				end
			end
		end
	end

	local function updateVirtualNodeWithChildren(virtualNode, hostParent, newChildElements)
		updateChildren(virtualNode, hostParent, newChildElements)
	end

	local function updateVirtualNodeWithRenderResult(virtualNode, hostParent, renderResult)
		if Type.of(renderResult) == Type.Element or renderResult == nil or typeof(renderResult) == "boolean" then
			updateChildren(virtualNode, hostParent, renderResult)
		else
			error(
				("%s\n%s"):format(
					"Component returned invalid children:",
					virtualNode.currentElement.source or "<enable element tracebacks>"
				),
				0
			)
		end
	end

	--[[
		Unmounts the given virtual node and releases any held resources.
	]]
	function unmountVirtualNode(virtualNode)
		if config.internalTypeChecks then
			internalAssert(Type.of(virtualNode) == Type.VirtualNode, "Expected arg #1 to be of type VirtualNode")
		end

		if virtualNode.wasUnmounted then
			return
		end

		virtualNode.wasUnmounted = true
		-- Invalidate child passes before callbacks can yield or re-enter reconciliation.
		virtualNode.updateChildrenCount += 1

		local kind = ElementKind.of(virtualNode.currentElement)

		-- selene: allow(if_same_then_else)
		if kind == ElementKind.Host then
			renderer.unmountHostNode(reconciler, virtualNode)
		elseif kind == ElementKind.Function then
			for _, childNode in pairs(virtualNode.children) do
				unmountVirtualNode(childNode)
			end
		elseif kind == ElementKind.Stateful then
			virtualNode.instance:__unmount()
		elseif kind == ElementKind.Portal then
			for _, childNode in pairs(virtualNode.children) do
				unmountVirtualNode(childNode)
			end
		elseif kind == ElementKind.Fragment then
			for _, childNode in pairs(virtualNode.children) do
				unmountVirtualNode(childNode)
			end
		else
			error(("Unknown ElementKind %q"):format(tostring(kind)), 2)
		end
	end

	local function updateFunctionVirtualNode(virtualNode, newElement)
		local updateCount = virtualNode.updateCount
		local children = newElement.component(newElement.props)

		if isUpdateCurrent(virtualNode, updateCount) then
			updateVirtualNodeWithRenderResult(virtualNode, virtualNode.hostParent, children)
		end

		return virtualNode
	end

	local function updatePortalVirtualNode(virtualNode, newElement)
		local oldElement = virtualNode.currentElement
		local oldTargetHostParent = oldElement.props.target

		local targetHostParent = newElement.props.target

		assert(renderer.isHostObject(targetHostParent), "Expected target to be host object")

		if targetHostParent ~= oldTargetHostParent then
			return replaceVirtualNode(virtualNode, newElement)
		end

		local children = newElement.props[Children]

		updateVirtualNodeWithChildren(virtualNode, targetHostParent, children)

		return virtualNode
	end

	local function updateFragmentVirtualNode(virtualNode, newElement)
		updateVirtualNodeWithChildren(virtualNode, virtualNode.hostParent, newElement.elements)

		return virtualNode
	end

	--[[
		Update the given virtual node using a new element describing what it
		should transform into.

		`updateVirtualNode` will return a new virtual node that should replace
		the passed in virtual node. This is because a virtual node can be
		updated with an element referencing a different component!

		In that case, `updateVirtualNode` will unmount the input virtual node,
		mount a new virtual node, and return it in this case, while also issuing
		a warning to the user.
	]]
	function updateVirtualNode(virtualNode, newElement, newState: { [any]: any }?): { [any]: any }?
		if config.internalTypeChecks then
			internalAssert(Type.of(virtualNode) == Type.VirtualNode, "Expected arg #1 to be of type VirtualNode")
		end
		if config.typeChecks then
			assert(
				Type.of(newElement) == Type.Element or typeof(newElement) == "boolean" or newElement == nil,
				"Expected arg #2 to be of type Element, boolean, or nil"
			)
		end

		if not isUpdateCurrent(virtualNode, virtualNode.updateCount) then
			return nil
		end

		local wasUpdating = virtualNode.activeUpdate ~= nil
		local updateCount = beginUpdate(virtualNode)

		-- Equality only proves a no-op when no older render has partially applied it.
		if virtualNode.currentElement == newElement and newState == nil and not wasUpdating then
			virtualNode.activeUpdate = nil
			return virtualNode
		end

		if typeof(newElement) == "boolean" or newElement == nil then
			unmountVirtualNode(virtualNode)
			return nil
		end

		if virtualNode.currentElement.component ~= newElement.component then
			return replaceVirtualNode(virtualNode, newElement)
		end

		local kind = ElementKind.of(newElement)

		local shouldContinueUpdate = true

		if kind == ElementKind.Host then
			virtualNode = renderer.updateHostNode(reconciler, virtualNode, newElement)
		elseif kind == ElementKind.Function then
			virtualNode = updateFunctionVirtualNode(virtualNode, newElement)
		elseif kind == ElementKind.Stateful then
			shouldContinueUpdate = virtualNode.instance:__update(newElement, newState, updateCount)
		elseif kind == ElementKind.Portal then
			virtualNode = updatePortalVirtualNode(virtualNode, newElement)
		elseif kind == ElementKind.Fragment then
			virtualNode = updateFragmentVirtualNode(virtualNode, newElement)
		else
			error(("Unknown ElementKind %q"):format(tostring(kind)), 2)
		end

		-- Rendering or reconciling children can yield while an ancestor is unmounted.
		if virtualNode == nil or virtualNode.wasUnmounted then
			return nil
		end

		if not isUpdateCurrent(virtualNode, updateCount) then
			return virtualNode
		end

		virtualNode.activeUpdate = nil

		-- Stateful components can abort updates via shouldUpdate. If that
		-- happens, we should stop doing stuff at this point.
		if not shouldContinueUpdate then
			return virtualNode
		end

		virtualNode.currentElement = newElement

		return virtualNode
	end

	--[[
		Constructs a new virtual node but not does mount it.
	]]
	local function createVirtualNode(element, hostParent, hostKey, context, legacyContext)
		if config.internalTypeChecks then
			internalAssert(
				renderer.isHostObject(hostParent) or hostParent == nil,
				"Expected arg #2 to be a host object"
			)
			internalAssert(typeof(context) == "table" or context == nil, "Expected arg #4 to be of type table or nil")
			internalAssert(
				typeof(legacyContext) == "table" or legacyContext == nil,
				"Expected arg #5 to be of type table or nil"
			)
		end
		if config.typeChecks then
			assert(hostKey ~= nil, "Expected arg #3 to be non-nil")
			assert(
				Type.of(element) == Type.Element or typeof(element) == "boolean",
				"Expected arg #1 to be of type Element or boolean"
			)
		end

		return {
			[Type] = Type.VirtualNode,
			currentElement = element,
			depth = 1,
			parent = nil,
			children = {},
			hostParent = hostParent,
			hostKey = hostKey,
			activeUpdate = 0,
			updateCount = 0,
			updateChildrenCount = 0,
			wasUnmounted = false,

			-- Legacy Context API
			-- A table of context values inherited from the parent node
			legacyContext = legacyContext,

			-- A saved copy of the parent context, used when replacing a node
			parentLegacyContext = legacyContext,

			-- Context API
			-- A table of context values inherited from the parent node
			context = context or {},

			-- A saved copy of the unmodified context; this will be updated when
			-- a component adds new context and used when a node is replaced
			originalContext = nil,
		}
	end

	local function mountFunctionVirtualNode(virtualNode)
		local element = virtualNode.currentElement

		local updateCount = virtualNode.updateCount
		local children = element.component(element.props)

		if isUpdateCurrent(virtualNode, updateCount) then
			updateVirtualNodeWithRenderResult(virtualNode, virtualNode.hostParent, children)
		end
	end

	local function mountPortalVirtualNode(virtualNode)
		local element = virtualNode.currentElement

		local targetHostParent = element.props.target
		local children = element.props[Children]

		assert(renderer.isHostObject(targetHostParent), "Expected target to be host object")

		updateVirtualNodeWithChildren(virtualNode, targetHostParent, children)
	end

	local function mountFragmentVirtualNode(virtualNode)
		local element = virtualNode.currentElement
		local children = element.elements

		updateVirtualNodeWithChildren(virtualNode, virtualNode.hostParent, children)
	end

	--[[
		Constructs a new virtual node and mounts it, but does not place it into
		the tree.
	]]
	function mountVirtualNode(element, hostParent, hostKey, context, legacyContext, parent, treeData)
		if config.internalTypeChecks then
			internalAssert(
				renderer.isHostObject(hostParent) or hostParent == nil,
				"Expected arg #2 to be a host object"
			)
			internalAssert(
				typeof(legacyContext) == "table" or legacyContext == nil,
				"Expected arg #5 to be of type table or nil"
			)
		end
		if config.typeChecks then
			assert(hostKey ~= nil, "Expected arg #3 to be non-nil")
			assert(
				Type.of(element) == Type.Element or typeof(element) == "boolean",
				"Expected arg #1 to be of type Element or boolean"
			)
		end

		-- Boolean values render as nil to enable terse conditional rendering.
		if typeof(element) == "boolean" then
			return nil
		end

		local kind = ElementKind.of(element)

		local virtualNode = createVirtualNode(element, hostParent, hostKey, context, legacyContext)
		virtualNode.parent = parent
		virtualNode.mountParentUpdateChildrenCount = parent and parent.updateChildrenCount
		virtualNode.treeData = treeData
		virtualNode.mountTreeUpdateCount = treeData and treeData.updateCount

		if kind == ElementKind.Host then
			renderer.mountHostNode(reconciler, virtualNode)
		elseif kind == ElementKind.Function then
			mountFunctionVirtualNode(virtualNode)
		elseif kind == ElementKind.Stateful then
			element.component:__mount(reconciler, virtualNode)
		elseif kind == ElementKind.Portal then
			mountPortalVirtualNode(virtualNode)
		elseif kind == ElementKind.Fragment then
			mountFragmentVirtualNode(virtualNode)
		else
			error(("Unknown ElementKind %q"):format(tostring(kind)), 2)
		end

		if not isUpdateCurrent(virtualNode, virtualNode.updateCount) then
			unmountVirtualNode(virtualNode)
		else
			virtualNode.mountParentUpdateChildrenCount = nil
			virtualNode.mountTreeUpdateCount = nil
			if virtualNode.updateCount == 0 then
				virtualNode.activeUpdate = nil
			end
		end
		return virtualNode
	end

	--[[
		Constructs a new Roact virtual tree, constructs a root node for
		it, and mounts it.
	]]
	local function mountVirtualTree(element, hostParent, hostKey)
		if config.typeChecks then
			assert(Type.of(element) == Type.Element, "Expected arg #1 to be of type Element")
			assert(renderer.isHostObject(hostParent) or hostParent == nil, "Expected arg #2 to be a host object")
		end

		if hostKey == nil then
			hostKey = "RoactTree"
		end

		local tree = {
			[Type] = Type.VirtualTree,
			[InternalData] = {
				-- The root node of the tree, which starts into the hierarchy of
				-- Roact component instances.
				rootNode = nil,
				mounted = true,
				updateCount = 0,
			},
		}

		tree[InternalData].rootNode = mountVirtualNode(element, hostParent, hostKey, nil, nil, nil, tree[InternalData])

		return tree
	end

	--[[
		Unmounts the virtual tree, freeing all of its resources.

		No further operations should be done on the tree after it's been
		unmounted, as indicated by its the `mounted` field.
	]]
	local function unmountVirtualTree(tree)
		local internalData = tree[InternalData]
		if config.typeChecks then
			assert(Type.of(tree) == Type.VirtualTree, "Expected arg #1 to be a Roact handle")
			assert(internalData.mounted, "Cannot unmounted a Roact tree that has already been unmounted")
		end

		internalData.mounted = false

		if internalData.rootNode ~= nil then
			unmountVirtualNode(internalData.rootNode)
		end
	end

	--[[
		Utility method for updating the root node of a virtual tree given a new
		element.
	]]
	local function updateVirtualTree(tree, newElement)
		local internalData = tree[InternalData]
		if config.typeChecks then
			assert(Type.of(tree) == Type.VirtualTree, "Expected arg #1 to be a Roact handle")
			assert(Type.of(newElement) == Type.Element, "Expected arg #2 to be a Roact Element")
		end

		if not internalData.mounted then
			return tree
		end

		internalData.updateCount = (internalData.updateCount or 0) + 1
		local updateCount = internalData.updateCount
		local rootNode = internalData.rootNode
		local newNode
		-- A reentrant update can arrive while a root replacement is still mounting.
		if rootNode.wasUnmounted then
			newNode = replaceVirtualNode(rootNode, newElement)
		else
			newNode = updateVirtualNode(rootNode, newElement)
		end

		-- The tree may have been unmounted or acquired a newer root while we yielded.
		if not internalData.mounted or internalData.updateCount ~= updateCount or internalData.rootNode ~= rootNode then
			if newNode and newNode ~= internalData.rootNode then
				unmountVirtualNode(newNode)
			end
		else
			internalData.rootNode = newNode
		end

		return tree
	end

	reconciler = {
		beginUpdate = beginUpdate,
		isUpdateCurrent = isUpdateCurrent,
		mountVirtualTree = mountVirtualTree,
		unmountVirtualTree = unmountVirtualTree,
		updateVirtualTree = updateVirtualTree,

		createVirtualNode = createVirtualNode,
		mountVirtualNode = mountVirtualNode,
		unmountVirtualNode = unmountVirtualNode,
		updateVirtualNode = updateVirtualNode,
		updateVirtualNodeWithChildren = updateVirtualNodeWithChildren,
		updateVirtualNodeWithRenderResult = updateVirtualNodeWithRenderResult,
	}

	return reconciler
end

return createReconciler
