--[[
	Deterministic lifetime schedules for the project-owned Roact reconciler override.
	Only the renderer boundary is injected. Components, elements, and bindings are Roact's.
]]
return function()
	local Roact = script.Parent.Parent.Packages.Roact
	local Binding = require(Roact.Binding)
	local Children = require(Roact.PropMarkers.Children)
	local Component = require(Roact.Component)
	local Ref = require(Roact.PropMarkers.Ref)
	local RobloxRenderer = require(Roact.RobloxRenderer)
	local createElement = require(Roact.createElement)
	local createFragment = require(Roact.createFragment)
	local createReconciler = require(Roact.createReconciler)

	local cleanups
	beforeEach(function()
		cleanups = {}
	end)
	afterEach(function()
		for index = #cleanups, 1, -1 do
			cleanups[index]()
		end
	end)

	local function resume(thread, expectedStatus)
		local success, message = coroutine.resume(thread)
		assert(success, tostring(message))
		expect(coroutine.status(thread)).to.equal(expectedStatus)
	end

	local function treeData(tree)
		for _, value in pairs(tree) do
			if type(value) == "table" and value.mounted ~= nil then
				return value
			end
		end
		error("Expected Roact tree ownership data.")
	end

	local function fakeRenderer()
		local nodes = {}
		local renderer = {}
		function renderer.isHostObject(target)
			return target == nil
		end
		function renderer.mountHostNode(reconciler, node)
			table.insert(nodes, node)
			node.testUnmounts = 0
			node.testUpdates = 0
			reconciler.updateVirtualNodeWithChildren(node, nil, node.currentElement.props[Children])
		end
		function renderer.updateHostNode(reconciler, node, element)
			assert(node.testUnmounts == 0, "Updated an unmounted host.")
			node.testUpdates += 1
			reconciler.updateVirtualNodeWithChildren(node, nil, element.props[Children])
			return node
		end
		function renderer.unmountHostNode(reconciler, node)
			node.testUnmounts += 1
			assert(node.testUnmounts == 1, "Unmounted a host twice.")
			for _, child in pairs(node.children) do
				reconciler.unmountVirtualNode(child)
			end
		end
		return renderer, nodes
	end

	local function expectAllUnmounted(nodes)
		for _, node in ipairs(nodes) do
			expect(node.wasUnmounted).to.equal(true)
			expect(node.testUnmounts).to.equal(1)
		end
	end

	it("refuses post-unmount updates and child passes, including the identical element fast path", function()
		local renderer, nodes = fakeRenderer()
		local reconciler = createReconciler(renderer)
		local element = createElement("Frame")
		local node = reconciler.mountVirtualNode(element, nil, "Root")
		reconciler.unmountVirtualNode(node)
		reconciler.updateVirtualNodeWithChildren(node, nil, { Child = createElement("UIPadding") })
		expect(reconciler.updateVirtualNode(node, element)).to.equal(nil)
		expect(reconciler.updateVirtualNode(node, createElement("TextLabel"))).to.equal(nil)
		reconciler.unmountVirtualNode(node)
		expect(node.currentElement).to.equal(element)
		expect((next(node.children))).to.equal(nil)
		expect(#nodes).to.equal(1)
		expectAllUnmounted(nodes)
	end)

	it("keeps an ordinary setState callback after unmount from touching retired children", function()
		local renderer, nodes = fakeRenderer()
		local reconciler = createReconciler(renderer)
		local View = Component:extend("RetiredCallback")
		function View:render()
			return createElement("UIPadding", { Value = self.state.value })
		end
		-- Deliberately no willUnmount method. Roact's lifecycle phase alone does not
		-- suppress setState for this otherwise ordinary component.
		local node = reconciler.mountVirtualNode(createElement(View), nil, "Root")
		local callback = function()
			node.instance:setState({ value = 2 })
		end
		reconciler.unmountVirtualNode(node)
		callback()
		expect(#nodes).to.equal(1)
		expect(nodes[1].testUpdates).to.equal(0)
		expectAllUnmounted(nodes)
	end)

	for _, stateful in ipairs({ false, true }) do
		local kind = if stateful then "stateful" else "function"
		for _, adding in ipairs({ false, true }) do
			local operation = if adding then "new mount" else "existing child update"
			it(`retires mapped UIPadding bindings across a paused {kind} {operation}`, function()
				local host = Instance.new("Frame")
				table.insert(cleanups, function()
					host:Destroy()
				end)
				local reconciler = createReconciler(RobloxRenderer)
				local binding, updateBinding = Binding.create(1)
				local evaluations = 0
				local detachments = 0
				local padding
				local function mappedPadding(value)
					evaluations += 1
					return UDim.new(0, value)
				end
				local function render(props)
					if props.pause then
						coroutine.yield()
					end
					return createElement("UIPadding", {
						PaddingTop = binding:map(mappedPadding),
						[Ref] = function(value)
							if value == nil then
								detachments += 1
							else
								padding = value
							end
						end,
					})
				end
				local View = render
				local unmounts = 0
				if stateful then
					View = Component:extend("PausedPadding")
					function View:render()
						return render(self.props)
					end
					function View:willUnmount()
						unmounts += 1
					end
				end
				local oldElement = createFragment(if adding then {} else { Child = createElement(View) })
				local node = reconciler.mountVirtualNode(oldElement, host, "Root")
				local child = node.children.Child
				local oldChildElement = if child then child.currentElement else nil
				local thread = coroutine.create(function()
					local result = reconciler.updateVirtualNode(
						node,
						createFragment({ Child = createElement(View, { pause = true }) })
					)
					expect(result).to.equal(nil)
				end)
				resume(thread, "suspended")
				reconciler.unmountVirtualNode(node)
				resume(thread, "dead")
				expect(node.currentElement).to.equal(oldElement)
				if child then
					expect(child.currentElement).to.equal(oldChildElement)
				else
					expect(node.children.Child).to.equal(nil)
				end
				expect(#host:GetChildren()).to.equal(0)
				if adding then
					expect(padding).to.equal(nil)
				else
					expect(padding.Parent).to.equal(nil)
				end
				expect(detachments).to.equal(if adding then 0 else 1)
				if stateful then
					expect(unmounts).to.equal(1)
				end
				-- Every bound write evaluates the map first. No evaluation after retirement
				-- proves there is neither a leaked subscription nor a stale property write,
				-- without reading properties that Lune invalidates on destroyed instances.
				local retiredEvaluations = evaluations
				updateBinding(9)
				expect(evaluations).to.equal(retiredEvaluations)
				reconciler.unmountVirtualNode(node)
				expect(detachments).to.equal(if adding then 0 else 1)
			end)
		end
	end

	for _, removeParent in ipairs({ false, true }) do
		local action = if removeParent then "unmounts its parent" else "re-enters its parent's child pass"
		it(`does not mount a replacement when willUnmount synchronously {action}`, function()
			local renderer, nodes = fakeRenderer()
			local reconciler = createReconciler(renderer)
			local parent
			local unmounts = 0
			local Old = Component:extend("RemovedDuringReplacement")
			function Old:render()
				return createElement("UIPadding")
			end
			function Old:willUnmount()
				unmounts += 1
				if removeParent then
					reconciler.unmountVirtualNode(parent)
				else
					reconciler.updateVirtualNodeWithChildren(parent, nil, {})
				end
			end
			parent = reconciler.mountVirtualNode(createFragment({ Child = createElement(Old) }), nil, "Root")
			reconciler.updateVirtualNodeWithChildren(parent, nil, { Child = createElement("TextLabel") })
			expect(unmounts).to.equal(1)
			expect(#nodes).to.equal(1)
			if not removeParent then
				expect((next(parent.children))).to.equal(nil)
				reconciler.unmountVirtualNode(parent)
			end
			expectAllUnmounted(nodes)
		end)
	end

	it("cleans an unpublished replacement after its mount unmounts the parent", function()
		local renderer, nodes = fakeRenderer()
		local reconciler = createReconciler(renderer)
		local parent = reconciler.mountVirtualNode(createFragment({ Child = createElement("Frame") }), nil, "Root")
		local function Replacement()
			reconciler.unmountVirtualNode(parent)
			return createElement("UIPadding")
		end
		reconciler.updateVirtualNodeWithChildren(parent, nil, { Child = createElement(Replacement) })
		expect(#nodes).to.equal(1)
		expectAllUnmounted(nodes)
	end)

	it("preserves a newer reentrant child pass and cleans only the stale mounted result", function()
		local renderer, nodes = fakeRenderer()
		local reconciler = createReconciler(renderer)
		local parent = reconciler.mountVirtualNode(createFragment({ Child = createElement("Frame") }), nil, "Root")
		local latest = createElement("TextLabel", { Value = "Latest" })
		local function Replacement()
			reconciler.updateVirtualNodeWithChildren(parent, nil, { Child = latest })
			return createElement("UIPadding")
		end
		reconciler.updateVirtualNodeWithChildren(parent, nil, { Child = createElement(Replacement) })
		expect(parent.children.Child.currentElement).to.equal(latest)
		expect(parent.children.Child.wasUnmounted).to.equal(false)
		reconciler.updateVirtualNodeWithChildren(
			parent,
			nil,
			{ Child = createElement("TextLabel", { Value = "Healthy" }) }
		)
		expect(parent.children.Child.testUpdates).to.equal(1)
		reconciler.unmountVirtualNode(parent)
		expectAllUnmounted(nodes)
	end)

	it("does not publish a root replacement that resumes after tree unmount", function()
		local renderer, nodes = fakeRenderer()
		local reconciler = createReconciler(renderer)
		local tree = reconciler.mountVirtualTree(createElement("Frame"))
		local original = treeData(tree).rootNode
		local function Replacement()
			coroutine.yield()
			return createElement("UIPadding")
		end
		local thread = coroutine.create(function()
			reconciler.updateVirtualTree(tree, createElement(Replacement))
		end)
		resume(thread, "suspended")
		reconciler.unmountVirtualTree(tree)
		resume(thread, "dead")
		expect(treeData(tree).rootNode).to.equal(original)
		expect(treeData(tree).mounted).to.equal(false)
		reconciler.updateVirtualTree(tree, createElement("TextLabel"))
		expect(#nodes).to.equal(1)
		expectAllUnmounted(nodes)
	end)

	it("preserves a newer root replacement during synchronous mount reentry", function()
		local renderer, nodes = fakeRenderer()
		local reconciler = createReconciler(renderer)
		local tree = reconciler.mountVirtualTree(createElement("Frame"))
		local latest = createElement("TextLabel")
		local function Replacement()
			reconciler.updateVirtualTree(tree, latest)
			return createElement("UIPadding")
		end
		reconciler.updateVirtualTree(tree, createElement(Replacement))
		expect(treeData(tree).rootNode.currentElement).to.equal(latest)
		expect(treeData(tree).rootNode.wasUnmounted).to.equal(false)
		reconciler.unmountVirtualTree(tree)
		expectAllUnmounted(nodes)
	end)

	for _, stateful in ipairs({ false, true }) do
		for _, nested in ipairs({ false, true }) do
			for _, yielding in ipairs({ false, true }) do
				it(
					`keeps the latest same-component render (stateful={stateful}, nested={nested}, yielding={yielding})`,
					function()
						local host = Instance.new("Folder")
						table.insert(cleanups, function()
							host:Destroy()
						end)
						local reconciler = createReconciler(RobloxRenderer)
						local tree
						local latest
						local function render(props)
							local value = props.value
							if value == "Old" then
								if yielding then
									coroutine.yield()
								else
									reconciler.updateVirtualTree(tree, latest)
								end
							end
							return createElement("StringValue", { Value = value })
						end
						local View = render
						if stateful then
							View = Component:extend("LatestRender")
							function View:render()
								return render(self.props)
							end
						end
						local function element(value)
							local child = createElement(View, { value = value })
							return if nested then createFragment({ Child = child }) else child
						end
						tree = reconciler.mountVirtualTree(element("Initial"), host)
						latest = element("Latest")
						local old = coroutine.create(function()
							reconciler.updateVirtualTree(tree, element("Old"))
						end)
						resume(old, if yielding then "suspended" else "dead")
						if yielding then
							reconciler.updateVirtualTree(tree, latest)
							resume(old, "dead")
						end
						local root = treeData(tree).rootNode
						local node = if nested then root.children.Child else root
						expect(root.currentElement).to.equal(latest)
						expect(node.currentElement.props.value).to.equal("Latest")
						if stateful then
							expect(node.instance.props.value).to.equal("Latest")
						end
						expect(host:GetChildren()[1].Value).to.equal("Latest")
						reconciler.unmountVirtualTree(tree)
					end
				)
			end
		end
	end

	it("invalidates an internal state-only render before a newer props update", function()
		local renderer = fakeRenderer()
		local reconciler = createReconciler(renderer)
		local View = Component:extend("StateOwnership")
		function View:render()
			local value = self.props.value
			if self.state.pause and value == "Initial" then
				coroutine.yield()
			end
			return createElement("StringValue", { Value = value })
		end
		local tree = reconciler.mountVirtualTree(createElement(View, { value = "Initial" }))
		local node = treeData(tree).rootNode
		local thread = coroutine.create(function()
			node.instance:setState({ pause = true })
		end)
		resume(thread, "suspended")
		local latest = createElement(View, { value = "Latest" })
		reconciler.updateVirtualTree(tree, latest)
		resume(thread, "dead")
		expect(node.currentElement).to.equal(latest)
		expect(node.instance.props.value).to.equal("Latest")
		for _, child in pairs(node.children) do
			expect(child.currentElement.props.Value).to.equal("Latest")
		end
		reconciler.unmountVirtualTree(tree)
	end)

	for _, stateful in ipairs({ false, true }) do
		it(`restores the identical committed element over an in-flight render (stateful={stateful})`, function()
			local host = Instance.new("Folder")
			table.insert(cleanups, function()
				host:Destroy()
			end)
			local reconciler = createReconciler(RobloxRenderer)
			local function render(props)
				if props.value == "Old" then
					coroutine.yield()
				end
				return createElement("StringValue", { Value = props.value })
			end
			local View = render
			if stateful then
				View = Component:extend("IdenticalCommittedElement")
				function View:render()
					return render(self.props)
				end
			end
			local committed = createElement(View, { value = "Latest" })
			local tree = reconciler.mountVirtualTree(committed, host)
			local thread = coroutine.create(function()
				reconciler.updateVirtualTree(tree, createElement(View, { value = "Old" }))
			end)
			resume(thread, "suspended")
			reconciler.updateVirtualTree(tree, committed)
			resume(thread, "dead")
			local node = treeData(tree).rootNode
			expect(node.currentElement).to.equal(committed)
			if stateful then
				expect(node.instance.props.value).to.equal("Latest")
			end
			expect(host:GetChildren()[1].Value).to.equal("Latest")
			reconciler.unmountVirtualTree(tree)
		end)
	end

	local function componentData(instance)
		for _, value in pairs(instance) do
			if type(value) == "table" and value.lifecyclePhase ~= nil then
				return value
			end
		end
		error("Expected component lifecycle data.")
	end

	for _, mounting in ipairs({ false, true }) do
		local phases = if mounting
			then { "init", "render", "reconcile", "didMount" }
			else { "shouldUpdate", "willUpdate", "render", "reconcile", "didUpdate" }
		for _, phase in ipairs(phases) do
			for _, yielding in ipairs({ false, true }) do
				it(
					`does not resurrect a retired lifecycle (mount={mounting}, phase={phase}, yielding={yielding})`,
					function()
						local renderer, nodes = fakeRenderer()
						local reconciler = createReconciler(renderer)
						local node
						local instance
						local armed = mounting
						local callbacks = {}
						local resource = false
						local function boundary(name)
							if armed then
								table.insert(callbacks, name)
								if name == phase then
									if yielding then
										coroutine.yield()
									else
										reconciler.unmountVirtualNode(node)
									end
								end
							end
						end
						local View = Component:extend("RetiredLifecycle")
						function View:init()
							instance = self
							node = componentData(self).virtualNode
							boundary("init")
						end
						function View:shouldUpdate()
							boundary("shouldUpdate")
							return true
						end
						function View:willUpdate()
							boundary("willUpdate")
						end
						function View:render()
							boundary("render")
							return createElement(function()
								boundary("reconcile")
								return createElement("Frame")
							end)
						end
						function View:didMount()
							self:setState({ pending = true })
							boundary("didMount")
						end
						function View:didUpdate()
							resource = true
							boundary("didUpdate")
						end
						local unmounts = 0
						function View:willUnmount()
							resource = false
							unmounts += 1
						end
						if not mounting then
							node = reconciler.mountVirtualNode(createElement(View), nil, "Root")
							armed = true
						end
						local thread = coroutine.create(function()
							if mounting then
								reconciler.mountVirtualNode(createElement(View), nil, "Root")
							else
								reconciler.updateVirtualNode(node, createElement(View, { updated = true }))
							end
						end)
						resume(thread, if yielding then "suspended" else "dead")
						if yielding then
							reconciler.unmountVirtualNode(node)
							resume(thread, "dead")
						end
						expect(callbacks[#callbacks]).to.equal(phase)
						expect(unmounts).to.equal(1)
						expect(resource).to.equal(false)
						local data = componentData(instance)
						expect(data.lifecyclePhase).to.equal(require(Roact.ComponentLifecyclePhase).WillUnmount)
						expect(data.pendingState).to.equal(nil)
						local called = false
						instance:setState(function()
							called = true
							return {}
						end)
						expect(called).to.equal(false)
						expectAllUnmounted(nodes)
					end
				)
			end
		end
	end

	for _, retiring in ipairs({ false, true }) do
		it(
			`cancels unpublished child mount lifecycles when the parent loses ownership (retiring={retiring})`,
			function()
				local renderer, nodes = fakeRenderer()
				local reconciler = createReconciler(renderer)
				local parent = reconciler.mountVirtualNode(createFragment({}), nil, "Root")
				local didMounts = 0
				local View = Component:extend("UnpublishedLifecycle")
				function View:render()
					coroutine.yield()
					return createElement("Frame")
				end
				function View:didMount()
					didMounts += 1
				end
				local thread = coroutine.create(function()
					reconciler.updateVirtualNodeWithChildren(parent, nil, { Child = createElement(View) })
				end)
				resume(thread, "suspended")
				if retiring then
					reconciler.unmountVirtualNode(parent)
				else
					reconciler.updateVirtualNodeWithChildren(parent, nil, { Latest = createElement("Frame") })
				end
				resume(thread, "dead")
				expect(didMounts).to.equal(0)
				if not retiring then
					expect(parent.children.Child).to.equal(nil)
					expect(parent.children.Latest ~= nil).to.equal(true)
					reconciler.unmountVirtualNode(parent)
				end
				expectAllUnmounted(nodes)
			end
		)
	end

	for _, phase in ipairs({ "shouldUpdate", "willUpdate", "didUpdate" }) do
		for _, yielding in ipairs({ false, true }) do
			it(`discards a superseded {phase} continuation (yielding={yielding})`, function()
				local host = Instance.new("Folder")
				table.insert(cleanups, function()
					host:Destroy()
				end)
				local reconciler = createReconciler(RobloxRenderer)
				local tree
				local latest
				local function boundary(value)
					if value == "Old" then
						if yielding then
							coroutine.yield()
						else
							reconciler.updateVirtualTree(tree, latest)
						end
					end
				end
				local View = Component:extend("SupersededLifecycle")
				View[phase] = function(self, props)
					boundary(if phase == "didUpdate" then self.props.value else props.value)
					return true
				end
				function View:render()
					return createElement("StringValue", { Value = self.props.value })
				end
				tree = reconciler.mountVirtualTree(createElement(View, { value = "Initial" }), host)
				latest = createElement(View, { value = "Latest" })
				local thread = coroutine.create(function()
					reconciler.updateVirtualTree(tree, createElement(View, { value = "Old" }))
				end)
				resume(thread, if yielding then "suspended" else "dead")
				if yielding then
					reconciler.updateVirtualTree(tree, latest)
					resume(thread, "dead")
				end
				local node = treeData(tree).rootNode
				expect(node.currentElement).to.equal(latest)
				expect(node.instance.props.value).to.equal("Latest")
				expect(host:GetChildren()[1].Value).to.equal("Latest")
				reconciler.unmountVirtualTree(tree)
			end)
		end
	end

	for _, retiring in ipairs({ false, true }) do
		it(`cancels a state updater paused across newer ownership (retiring={retiring})`, function()
			local renderer = fakeRenderer()
			local reconciler = createReconciler(renderer)
			local View = Component:extend("PausedStateUpdater")
			function View:render()
				return createElement("Frame")
			end
			local tree = reconciler.mountVirtualTree(createElement(View, { value = "Initial" }))
			local node = treeData(tree).rootNode
			local thread = coroutine.create(function()
				node.instance:setState(function()
					coroutine.yield()
					return { stale = true }
				end)
			end)
			resume(thread, "suspended")
			if retiring then
				reconciler.unmountVirtualTree(tree)
			else
				reconciler.updateVirtualTree(tree, createElement(View, { value = "Latest" }))
			end
			resume(thread, "dead")
			expect(node.instance.state.stale).to.equal(nil)
			expect(componentData(node.instance).pendingState).to.equal(nil)
			if not retiring then
				expect(node.instance.props.value).to.equal("Latest")
				reconciler.unmountVirtualTree(tree)
			end
		end)
	end

	for _, boundary in ipairs({ "old", "new" }) do
		for _, yielding in ipairs({ false, true }) do
			for _, retiring in ipairs({ false, true }) do
				it(
					`owns refs and bindings during {boundary} ref reentry (yielding={yielding}, retiring={retiring})`,
					function()
						local host = Instance.new("Folder")
						table.insert(cleanups, function()
							host:Destroy()
						end)
						local reconciler = createReconciler(RobloxRenderer)
						local tree
						local latest
						local armed = false
						local counts = { old = 0, new = 0, latest = 0 }
						local staleBinding, updateStale = Binding.create("Old")
						local liveBinding, updateLive = Binding.create("Latest")
						local staleReads = 0
						local liveReads = 0
						local stale = staleBinding:map(function(value)
							staleReads += 1
							return value
						end)
						local live = liveBinding:map(function(value)
							liveReads += 1
							return value
						end)
						local function reenter()
							if yielding then
								coroutine.yield()
							elseif retiring then
								reconciler.unmountVirtualTree(tree)
							else
								reconciler.updateVirtualTree(tree, latest)
							end
						end
						local function oldRef(value)
							if value == nil then
								counts.old += 1
								if armed and boundary == "old" then
									armed = false
									reenter()
								end
							end
						end
						local function newRef(value)
							if value == nil then
								counts.new += 1
							elseif armed and boundary == "new" then
								armed = false
								reenter()
							end
						end
						latest = createElement("StringValue", {
							Value = live,
							[Ref] = function(value)
								if value == nil then
									counts.latest += 1
								end
							end,
						})
						tree = reconciler.mountVirtualTree(createElement("StringValue", { [Ref] = oldRef }), host)
						local node = treeData(tree).rootNode
						armed = true
						local thread = coroutine.create(function()
							reconciler.updateVirtualTree(
								tree,
								createElement("StringValue", { Value = stale, [Ref] = newRef })
							)
						end)
						resume(thread, if yielding then "suspended" else "dead")
						if yielding then
							if retiring then
								reconciler.unmountVirtualTree(tree)
							else
								reconciler.updateVirtualTree(tree, latest)
							end
							resume(thread, "dead")
						end
						expect(counts.old).to.equal(1)
						expect(counts.new).to.equal(if boundary == "new" then 1 else 0)
						local before = staleReads
						updateStale("Stale")
						expect(staleReads).to.equal(before)
						if not retiring then
							expect(node.currentElement).to.equal(latest)
							expect(node.hostObject.Value).to.equal("Latest")
							before = liveReads
							updateLive("Healthy")
							expect(liveReads).to.equal(before + 1)
							expect(node.hostObject.Value).to.equal("Healthy")
							reconciler.unmountVirtualTree(tree)
							expect(counts.latest).to.equal(1)
						end
						expect(node.bindings).to.equal(nil)
						expect(#host:GetChildren()).to.equal(0)
					end
				)
			end
		end
	end

	for _, retiring in ipairs({ false, true }) do
		for _, yielding in ipairs({ false, true }) do
			it(
				`cancels initial binding reads across host reentry (retiring={retiring}, yielding={yielding})`,
				function()
					local host = Instance.new("Folder")
					table.insert(cleanups, function()
						host:Destroy()
					end)
					local reconciler = createReconciler(RobloxRenderer)
					local tree = reconciler.mountVirtualTree(createElement("StringValue", { Value = "Initial" }), host)
					local node = treeData(tree).rootNode
					local latest = createElement("StringValue", { Value = "Latest" })
					local binding, update = Binding.create("Old")
					local reads = 0
					local mapped = binding:map(function(value)
						reads += 1
						if yielding then
							coroutine.yield()
						elseif retiring then
							reconciler.unmountVirtualTree(tree)
						else
							reconciler.updateVirtualTree(tree, latest)
						end
						return value
					end)
					local thread = coroutine.create(function()
						reconciler.updateVirtualTree(tree, createElement("StringValue", { Value = mapped }))
					end)
					resume(thread, if yielding then "suspended" else "dead")
					if yielding then
						if retiring then
							reconciler.unmountVirtualTree(tree)
						else
							reconciler.updateVirtualTree(tree, latest)
						end
						resume(thread, "dead")
					end
					local before = reads
					update("Unused")
					expect(reads).to.equal(before)
					if not retiring then
						expect(node.currentElement).to.equal(latest)
						expect(node.hostObject.Value).to.equal("Latest")
						reconciler.unmountVirtualTree(tree)
					end
					expect(node.bindings).to.equal(nil)
				end
			)
		end
	end

	it("restores the committed host element when its old ref reenters with that identical element", function()
		local host = Instance.new("Folder")
		table.insert(cleanups, function()
			host:Destroy()
		end)
		local reconciler = createReconciler(RobloxRenderer)
		local tree
		local committed
		local armed = false
		local active
		local oldRef = function(value)
			active = value
			if value == nil and armed then
				armed = false
				reconciler.updateVirtualTree(tree, committed)
			end
		end
		committed = createElement("StringValue", { Value = "Latest", [Ref] = oldRef })
		tree = reconciler.mountVirtualTree(committed, host)
		armed = true
		local staleAttachments = 0
		reconciler.updateVirtualTree(
			tree,
			createElement("StringValue", {
				Value = "Old",
				[Ref] = function(value)
					if value ~= nil then
						staleAttachments += 1
					end
				end,
			})
		)
		expect(treeData(tree).rootNode.currentElement).to.equal(committed)
		expect(host:GetChildren()[1].Value).to.equal("Latest")
		expect(active).to.equal(host:GetChildren()[1])
		expect(staleAttachments).to.equal(0)
		reconciler.unmountVirtualTree(tree)
		expect(active).to.equal(nil)
	end)

	for _, yielding in ipairs({ false, true }) do
		it(`retires a host during its initial ref attachment (yielding={yielding})`, function()
			local host = Instance.new("Folder")
			table.insert(cleanups, function()
				host:Destroy()
			end)
			local reconciler = createReconciler(RobloxRenderer)
			local binding, update = Binding.create("Initial")
			local reads = 0
			local detachments = 0
			local node
			local element = createElement("StringValue", {
				Value = binding:map(function(value)
					reads += 1
					return value
				end),
				[Ref] = function(value)
					if value == nil then
						detachments += 1
					elseif yielding then
						coroutine.yield()
					else
						reconciler.unmountVirtualNode(node)
					end
				end,
			})
			node = reconciler.createVirtualNode(element, host, "Root")
			local thread = coroutine.create(function()
				RobloxRenderer.mountHostNode(reconciler, node)
			end)
			resume(thread, if yielding then "suspended" else "dead")
			if yielding then
				reconciler.unmountVirtualNode(node)
				resume(thread, "dead")
			end
			local before = reads
			update("Unused")
			expect(reads).to.equal(before)
			expect(detachments).to.equal(1)
			expect(node.bindings).to.equal(nil)
			expect(#host:GetChildren()).to.equal(0)
		end)
	end

	it("replaces healthy refs exactly once while retaining an unchanged binding", function()
		local host = Instance.new("Folder")
		table.insert(cleanups, function()
			host:Destroy()
		end)
		local reconciler = createReconciler(RobloxRenderer)
		local binding, update = Binding.create("Initial")
		local callbacks = {}
		local function ref(name)
			return function(value)
				table.insert(callbacks, name .. if value == nil then " off" else " on")
			end
		end
		local first = ref("first")
		local second = ref("second")
		local tree = reconciler.mountVirtualTree(createElement("StringValue", { Value = binding, [Ref] = first }), host)
		local element = createElement("StringValue", { Value = binding, [Ref] = second })
		reconciler.updateVirtualTree(tree, element)
		reconciler.updateVirtualTree(tree, element)
		update("Latest")
		expect(host:GetChildren()[1].Value).to.equal("Latest")
		reconciler.unmountVirtualTree(tree)
		expect(table.concat(callbacks, ",")).to.equal("first on,first off,second on,second off")
	end)

	it("retires partially published children when a newer host update has no children", function()
		local host = Instance.new("Folder")
		table.insert(cleanups, function()
			host:Destroy()
		end)
		local reconciler = createReconciler(RobloxRenderer)
		local tree = reconciler.mountVirtualTree(createElement("Folder"), host)
		local node = treeData(tree).rootNode
		local folder = node.hostObject
		local binding, update = Binding.create("Bound")
		local renders, mounts, unmounts, attachments, detachments, evaluations = 0, 0, 0, 0, 0, 0
		local View = Component:extend("PartiallyPublishedResource")
		function View:render()
			return createElement("StringValue", {
				Value = binding:map(function(value)
					evaluations += 1
					return value
				end),
				[Ref] = function(value)
					if value == nil then
						detachments += 1
					else
						attachments += 1
					end
				end,
			})
		end
		function View:didMount()
			mounts += 1
		end
		function View:willUnmount()
			unmounts += 1
		end
		local function Child()
			renders += 1
			-- Pause the second invocation, regardless of the child table's iteration order.
			if renders == 2 then
				coroutine.yield()
			end
			return createElement(View)
		end
		local thread = coroutine.create(function()
			reconciler.updateVirtualTree(
				tree,
				createElement("Folder", nil, {
					A = createElement(Child),
					B = createElement(Child),
				})
			)
		end)
		resume(thread, "suspended")
		expect(renders).to.equal(2)
		expect(#folder:GetChildren()).to.equal(1)
		expect(mounts).to.equal(1)
		expect(attachments).to.equal(1)
		local published = node.children[next(node.children)]
		local latest = createElement("Folder")
		reconciler.updateVirtualTree(tree, latest)
		local childrenBeforeResume = #folder:GetChildren()
		local unmountsBeforeResume = unmounts
		local detachmentsBeforeResume = detachments
		-- Finish the old pass before asserting so a failing run leaves no suspended work.
		resume(thread, "dead")
		expect(childrenBeforeResume).to.equal(0)
		expect(unmountsBeforeResume).to.equal(1)
		expect(detachmentsBeforeResume).to.equal(1)
		expect(node.currentElement).to.equal(latest)
		expect(#folder:GetChildren()).to.equal(0)
		expect((next(node.children))).to.equal(nil)
		expect(published.wasUnmounted).to.equal(true)
		expect(mounts).to.equal(1)
		expect(unmounts).to.equal(1)
		expect(attachments).to.equal(1)
		expect(detachments).to.equal(1)
		local before = evaluations
		update("Retired")
		expect(evaluations).to.equal(before)
		reconciler.unmountVirtualTree(tree)
		expect(unmounts).to.equal(1)
		expect(detachments).to.equal(1)
		expect(#host:GetChildren()).to.equal(0)
	end)

	for _, stateful in ipairs({ false, true }) do
		it(`finishes a reused binding's pending initial write (stateful={stateful})`, function()
			local host = Instance.new("Folder")
			table.insert(cleanups, function()
				host:Destroy()
			end)
			local reconciler = createReconciler(RobloxRenderer)
			local binding, update = Binding.create("Bound")
			local evaluations = 0
			local mapped = binding:map(function(value)
				evaluations += 1
				if evaluations == 1 then
					coroutine.yield()
				end
				return value
			end)
			local function render(props)
				return createElement("StringValue", { Value = props.value })
			end
			local View = render
			if stateful then
				View = Component:extend("ReusedPendingBinding")
				function View:render()
					return render(self.props)
				end
			end
			local tree = reconciler.mountVirtualTree(createElement(View, { value = "Initial" }), host)
			local instance = host:GetChildren()[1]
			local thread = coroutine.create(function()
				reconciler.updateVirtualTree(tree, createElement(View, { value = mapped }))
			end)
			resume(thread, "suspended")
			local root = treeData(tree).rootNode
			local hostNode = root.children[next(root.children)]
			local subscription = hostNode.bindings.Value
			local latest = createElement(View, { value = mapped })
			reconciler.updateVirtualTree(tree, latest)
			local beforeResume = instance.Value
			resume(thread, "dead")
			expect(hostNode.bindings.Value).to.equal(subscription)
			expect((next(hostNode.pendingBindings))).to.equal(nil)
			expect(beforeResume).to.equal("Bound")
			expect(instance.Value).to.equal("Bound")
			expect(treeData(tree).rootNode.currentElement).to.equal(latest)
			expect(evaluations).to.equal(2)
			-- Healthy reuse must neither re-read the map nor duplicate its observer.
			reconciler.updateVirtualTree(tree, createElement(View, { value = mapped }))
			expect(evaluations).to.equal(2)
			update("Emission")
			expect(evaluations).to.equal(3)
			expect(instance.Value).to.equal("Emission")
			reconciler.unmountVirtualTree(tree)
			update("Retired")
			expect(evaluations).to.equal(3)
			expect(#host:GetChildren()).to.equal(0)
		end)
	end

	for _, action in ipairs({ "remove", "replace", "unmount" }) do
		it(`cleans a pending binding initializer on {action}`, function()
			local host = Instance.new("Folder")
			table.insert(cleanups, function()
				host:Destroy()
			end)
			local reconciler = createReconciler(RobloxRenderer)
			local tree = reconciler.mountVirtualTree(createElement("StringValue", { Value = "Initial" }), host)
			local node = treeData(tree).rootNode
			local binding, update = Binding.create("Old")
			local evaluations = 0
			local mapped = binding:map(function(value)
				evaluations += 1
				if evaluations == 1 then
					coroutine.yield()
				end
				return value
			end)
			local thread = coroutine.create(function()
				reconciler.updateVirtualTree(tree, createElement("StringValue", { Value = mapped }))
			end)
			resume(thread, "suspended")
			if action == "unmount" then
				reconciler.unmountVirtualTree(tree)
			else
				reconciler.updateVirtualTree(
					tree,
					createElement("StringValue", if action == "replace" then { Value = "Latest" } else {})
				)
			end
			resume(thread, "dead")
			update("Stale")
			expect(evaluations).to.equal(1)
			expect(node.pendingBindings == nil or next(node.pendingBindings) == nil).to.equal(true)
			if action ~= "unmount" then
				expect(node.hostObject.Value).to.equal(if action == "replace" then "Latest" else "")
				reconciler.unmountVirtualTree(tree)
			end
			expect(node.bindings).to.equal(nil)
			expect(node.pendingBindings).to.equal(nil)
			expect(#host:GetChildren()).to.equal(0)
		end)
	end

	for _, olderFirst in ipairs({ false, true }) do
		it(`keeps a subscription emission over both owners' paused initial reads (olderFirst={olderFirst})`, function()
			local host = Instance.new("Folder")
			table.insert(cleanups, function()
				host:Destroy()
			end)
			local reconciler = createReconciler(RobloxRenderer)
			local tree = reconciler.mountVirtualTree(createElement("StringValue", { Value = "Initial" }), host)
			local node = treeData(tree).rootNode
			local instance = node.hostObject
			local binding, update = Binding.create("Bound")
			local evaluations = 0
			local mapped = binding:map(function(value)
				evaluations += 1
				if evaluations <= 2 then
					coroutine.yield()
				end
				return value
			end)
			local older = coroutine.create(function()
				reconciler.updateVirtualTree(tree, createElement("StringValue", { Value = mapped }))
			end)
			resume(older, "suspended")
			local subscription = node.bindings.Value
			local latest = createElement("StringValue", { Value = mapped })
			local newer = coroutine.create(function()
				reconciler.updateVirtualTree(tree, latest)
			end)
			resume(newer, "suspended")
			expect(evaluations).to.equal(2)
			expect(node.bindings.Value).to.equal(subscription)
			update("Emission")
			local emitted = instance.Value
			local pendingAfterEmission = node.pendingBindings.Value
			-- Complete both reads without a third owner invalidating the newer one.
			resume(if olderFirst then older else newer, "dead")
			resume(if olderFirst then newer else older, "dead")
			expect(emitted).to.equal("Emission")
			expect(instance.Value).to.equal("Emission")
			expect(pendingAfterEmission).to.equal(nil)
			expect(node.pendingBindings.Value).to.equal(nil)
			expect(node.currentElement).to.equal(latest)
			expect(node.bindings.Value).to.equal(subscription)
			expect(evaluations).to.equal(3)
			reconciler.updateVirtualTree(tree, createElement("StringValue", { Value = mapped }))
			expect(evaluations).to.equal(3)
			update("Latest")
			expect(evaluations).to.equal(4)
			expect(instance.Value).to.equal("Latest")
			reconciler.unmountVirtualTree(tree)
			update("Retired")
			expect(evaluations).to.equal(4)
			expect(node.bindings).to.equal(nil)
			expect(node.pendingBindings).to.equal(nil)
			expect(#host:GetChildren()).to.equal(0)
		end)
	end

	for _, replace in ipairs({ false, true }) do
		it(`does not clear a newer pending initializer when a stale read returns (replace={replace})`, function()
			local host = Instance.new("Folder")
			table.insert(cleanups, function()
				host:Destroy()
			end)
			local reconciler = createReconciler(RobloxRenderer)
			local tree = reconciler.mountVirtualTree(createElement("StringValue", { Value = "Initial" }), host)
			local binding, update = Binding.create("Bound")
			local evaluations = 0
			local function map(value)
				evaluations += 1
				if evaluations <= 2 then
					coroutine.yield()
				end
				return value
			end
			local first = binding:map(map)
			local second = if replace then binding:map(map) else first
			local old = coroutine.create(function()
				reconciler.updateVirtualTree(tree, createElement("StringValue", { Value = first }))
			end)
			resume(old, "suspended")
			local newer = coroutine.create(function()
				reconciler.updateVirtualTree(tree, createElement("StringValue", { Value = second }))
			end)
			resume(newer, "suspended")
			resume(old, "dead")
			-- A third owner still has to see the newer read as unfinished.
			reconciler.updateVirtualTree(tree, createElement("StringValue", { Value = second }))
			expect(host:GetChildren()[1].Value).to.equal("Bound")
			update("Emission")
			expect(evaluations).to.equal(4)
			resume(newer, "dead")
			expect(host:GetChildren()[1].Value).to.equal("Emission")
			local node = treeData(tree).rootNode
			expect((next(node.pendingBindings))).to.equal(nil)
			reconciler.unmountVirtualTree(tree)
			update("Retired")
			expect(evaluations).to.equal(4)
			expect(node.pendingBindings).to.equal(nil)
		end)
	end

	for _, reentrant in ipairs({ false, true }) do
		it(`keeps the latest mapped emission without host updates (reentrant={reentrant})`, function()
			local host = Instance.new("Folder")
			table.insert(cleanups, function()
				host:Destroy()
			end)
			local reconciler = createReconciler(RobloxRenderer)
			local binding, update = Binding.create("Bound")
			local evaluations = 0
			local mapped = binding:map(function(value)
				evaluations += 1
				if value == "Older" then
					if reentrant then
						update("Latest")
					else
						coroutine.yield()
					end
				end
				return value
			end)
			local tree = reconciler.mountVirtualTree(createElement("StringValue", { Value = mapped }), host)
			local instance = host:GetChildren()[1]
			local older = coroutine.create(function()
				update("Older")
			end)
			if reentrant then
				resume(older, "dead")
			else
				resume(older, "suspended")
				update("Latest")
				resume(older, "dead")
			end
			expect(instance.Value).to.equal("Latest")
			expect(evaluations).to.equal(3)
			update("Healthy")
			expect(instance.Value).to.equal("Healthy")
			expect(evaluations).to.equal(4)
			reconciler.unmountVirtualTree(tree)
			update("Retired")
			expect(evaluations).to.equal(4)
		end)
	end

	for _, reentrant in ipairs({ false, true }) do
		it(
			`keeps all mapped subscribers current after a root dispatch is superseded (reentrant={reentrant})`,
			function()
				local binding, update = Binding.create("Bound")
				local values = { "Initial", "Initial" }
				local disconnects = {}
				local paused = false
				local evaluations = 0
				table.insert(cleanups, function()
					for _, disconnect in disconnects do
						disconnect()
					end
					table.clear(disconnects)
				end)
				for index = 1, 2 do
					local mapped = binding:map(function(value)
						evaluations += 1
						if value == "Older" and not paused then
							paused = true
							if reentrant then
								update("Latest")
							else
								coroutine.yield()
							end
						end
						return value
					end)
					disconnects[index] = Binding.subscribe(mapped, function(value)
						values[index] = value
					end)
				end
				local older = coroutine.create(function()
					update("Older")
				end)
				resume(older, if reentrant then "dead" else "suspended")
				if not reentrant then
					update("Latest")
					resume(older, "dead")
				end
				expect(values[1]).to.equal("Latest")
				expect(values[2]).to.equal("Latest")
				local beforeHealthy = evaluations
				update("Healthy")
				expect(values[1]).to.equal("Healthy")
				expect(values[2]).to.equal("Healthy")
				expect(evaluations).to.equal(beforeHealthy + 2)
			end
		)
	end

	for _, olderFirst in ipairs({ false, true }) do
		it(`keeps the latest mapped emission over two paused initializers (olderFirst={olderFirst})`, function()
			local host = Instance.new("Folder")
			table.insert(cleanups, function()
				host:Destroy()
			end)
			local reconciler = createReconciler(RobloxRenderer)
			local tree = reconciler.mountVirtualTree(createElement("StringValue", { Value = "Initial" }), host)
			local node = treeData(tree).rootNode
			local binding, update = Binding.create("Bound")
			local evaluations = 0
			local mapped = binding:map(function(value)
				evaluations += 1
				if evaluations <= 2 or value == "Older" then
					coroutine.yield()
				end
				return value
			end)
			local older = coroutine.create(function()
				reconciler.updateVirtualTree(tree, createElement("StringValue", { Value = mapped }))
			end)
			resume(older, "suspended")
			local subscription = node.bindings.Value
			local latest = createElement("StringValue", { Value = mapped })
			local newer = coroutine.create(function()
				reconciler.updateVirtualTree(tree, latest)
			end)
			resume(newer, "suspended")
			local emission = coroutine.create(function()
				update("Older")
			end)
			resume(emission, "suspended")
			update("Latest")
			local emitted = node.hostObject.Value
			local pendingAfterEmission = node.pendingBindings.Value
			resume(if olderFirst then older else newer, "dead")
			resume(if olderFirst then newer else older, "dead")
			local afterInitializers = node.hostObject.Value
			resume(emission, "dead")
			expect(emitted).to.equal("Latest")
			expect(afterInitializers).to.equal("Latest")
			expect(node.hostObject.Value).to.equal("Latest")
			expect(pendingAfterEmission).to.equal(nil)
			expect(node.pendingBindings.Value).to.equal(nil)
			expect(node.currentElement).to.equal(latest)
			expect(node.bindings.Value).to.equal(subscription)
			expect(evaluations).to.equal(4)
			reconciler.updateVirtualTree(tree, createElement("StringValue", { Value = mapped }))
			expect(evaluations).to.equal(4)
			update("Healthy")
			expect(node.hostObject.Value).to.equal("Healthy")
			expect(evaluations).to.equal(5)
			reconciler.unmountVirtualTree(tree)
			update("Retired")
			expect(evaluations).to.equal(5)
			expect(node.bindings).to.equal(nil)
			expect(node.pendingBindings).to.equal(nil)
		end)
	end

	it("orders joined evaluations across different inputs when a mapped read suspends", function()
		local host = Instance.new("Folder")
		table.insert(cleanups, function()
			host:Destroy()
		end)
		local reconciler = createReconciler(RobloxRenderer)
		local first, updateFirst = Binding.create("Bound")
		local second, updateSecond = Binding.create("Initial")
		local pause = false
		local evaluations = 0
		local mapped = first:map(function(value)
			evaluations += 1
			if pause then
				pause = false
				coroutine.yield()
			end
			return value
		end)
		local joinedEvaluations = 0
		local joined = Binding.join({ first = mapped, second = second }):map(function(values)
			joinedEvaluations += 1
			return `{values.first}/{values.second}`
		end)
		local tree = reconciler.mountVirtualTree(createElement("StringValue", { Value = joined }), host)
		local instance = host:GetChildren()[1]
		pause = true
		local older = coroutine.create(function()
			updateSecond("Older")
		end)
		resume(older, "suspended")
		updateFirst("Latest")
		local emitted = instance.Value
		resume(older, "dead")
		expect(emitted).to.equal("Latest/Older")
		expect(instance.Value).to.equal("Latest/Older")
		expect(evaluations).to.equal(4)
		expect(joinedEvaluations).to.equal(2)
		updateSecond("Healthy")
		expect(instance.Value).to.equal("Latest/Healthy")
		expect(evaluations).to.equal(5)
		expect(joinedEvaluations).to.equal(3)
		reconciler.unmountVirtualTree(tree)
		updateFirst("Retired")
		updateSecond("Retired")
		expect(evaluations).to.equal(5)
		expect(joinedEvaluations).to.equal(3)
	end)

	for _, joined in ipairs({ false, true }) do
		it(`disconnects a subscription while its mapped evaluation is suspended (joined={joined})`, function()
			local binding, update = Binding.create("Bound")
			local trigger, updateTrigger = Binding.create("Initial")
			local pause = false
			local evaluations = 0
			local mapped = binding:map(function(value)
				evaluations += 1
				if pause then
					pause = false
					coroutine.yield()
				end
				return value
			end)
			local observed = if joined then Binding.join({ value = mapped, trigger = trigger }) else mapped
			local callbacks = 0
			local disconnect = Binding.subscribe(observed, function()
				callbacks += 1
			end)
			pause = true
			local older = coroutine.create(function()
				if joined then
					updateTrigger("Older")
				else
					update("Older")
				end
			end)
			resume(older, "suspended")
			disconnect()
			resume(older, "dead")
			local latest
			local disconnectNew = Binding.subscribe(observed, function(value)
				latest = if joined then value.value else value
			end)
			update("Latest")
			disconnectNew()
			local beforeRetired = evaluations
			update("Retired")
			updateTrigger("Retired")
			expect(callbacks).to.equal(0)
			expect(latest).to.equal("Latest")
			expect(evaluations).to.equal(beforeRetired)
		end)
	end

	it("propagates binding initialization errors and lets a newer owner retry the same binding", function()
		local host = Instance.new("Folder")
		table.insert(cleanups, function()
			host:Destroy()
		end)
		local reconciler = createReconciler(RobloxRenderer)
		local tree = reconciler.mountVirtualTree(createElement("StringValue", { Value = "Initial" }), host)
		local binding, update = Binding.create("Bound")
		local evaluations = 0
		local mapped = binding:map(function(value)
			evaluations += 1
			if evaluations == 1 then
				error("Expected binding initialization error.")
			end
			return value
		end)
		local success, message = pcall(function()
			reconciler.updateVirtualTree(tree, createElement("StringValue", { Value = mapped }))
		end)
		expect(success).to.equal(false)
		expect(string.find(tostring(message), "Expected binding initialization error.", 1, true) ~= nil).to.equal(true)
		reconciler.updateVirtualTree(tree, createElement("StringValue", { Value = mapped }))
		expect(host:GetChildren()[1].Value).to.equal("Bound")
		update("Emission")
		expect(evaluations).to.equal(3)
		expect(host:GetChildren()[1].Value).to.equal("Emission")
		reconciler.unmountVirtualTree(tree)
		update("Retired")
		expect(evaluations).to.equal(3)
	end)

	it("preserves lifecycle errors instead of treating them as cancellation", function()
		local renderer = fakeRenderer()
		local reconciler = createReconciler(renderer)
		local View = Component:extend("LifecycleError")
		function View:render()
			return createElement("Frame")
		end
		function View:didUpdate()
			error("Expected lifecycle error.")
		end
		local tree = reconciler.mountVirtualTree(createElement(View))
		local success, message = pcall(function()
			reconciler.updateVirtualTree(tree, createElement(View, { updated = true }))
		end)
		expect(success).to.equal(false)
		expect(string.find(tostring(message), "Expected lifecycle error.", 1, true) ~= nil).to.equal(true)
		reconciler.unmountVirtualTree(tree)
	end)

	it("allows healthy mounted binding and component updates", function()
		local host = Instance.new("Frame")
		table.insert(cleanups, function()
			host:Destroy()
		end)
		local reconciler = createReconciler(RobloxRenderer)
		local binding, updateBinding = Binding.create(1)
		local evaluations = 0
		local function View(props)
			return createElement("UIPadding", {
				PaddingTop = binding:map(function(value)
					evaluations += 1
					return UDim.new(0, value + props.offset)
				end),
			})
		end
		local tree = reconciler.mountVirtualTree(createElement(View, { offset = 0 }), host)
		local padding = host:GetChildren()[1]
		updateBinding(2)
		expect(padding.PaddingTop).to.equal(UDim.new(0, 2))
		reconciler.updateVirtualTree(tree, createElement(View, { offset = 3 }))
		expect(host:GetChildren()[1]).to.equal(padding)
		local before = evaluations
		updateBinding(4)
		expect(evaluations).to.equal(before + 1)
		expect(padding.PaddingTop).to.equal(UDim.new(0, 7))
		reconciler.unmountVirtualTree(tree)
		before = evaluations
		updateBinding(5)
		expect(evaluations).to.equal(before)
		expect(#host:GetChildren()).to.equal(0)
	end)
end
