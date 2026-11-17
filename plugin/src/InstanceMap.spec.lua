--[[

InstanceMap.spec

Tests which parent changes the instance map reports. Only an instance whose own Parent
changed should be reported, never the descendants of an instance that moved.

--]]

return function()
	const InstanceMap = require("./InstanceMap")

	const CLASS_NAMES = { "Folder", "StringValue" }

	-- Each action changes either the watched leaf's own parent or its ancestor's, starting
	-- from container > ancestor > leaf.
	const ACTIONS = {
		{
			Name = "move the leaf",
			OwnParent = true,
			Run = function(tree)
				tree.Leaf.Parent = tree.Container
			end,
		},
		{
			Name = "unparent the leaf",
			OwnParent = true,
			Run = function(tree)
				tree.Leaf.Parent = nil
			end,
		},
		{
			Name = "destroy the leaf",
			OwnParent = true,
			Run = function(tree)
				tree.Leaf:Destroy()
			end,
		},
		{
			Name = "move the ancestor",
			OwnParent = false,
			Run = function(tree)
				tree.Ancestor.Parent = tree.Other
			end,
		},
		{
			Name = "unparent the ancestor",
			OwnParent = false,
			Run = function(tree)
				tree.Ancestor.Parent = nil
			end,
		},
	}

	-- Creates a map that records every Parent change it reports. The test place runs tests
	-- in a running game, where the map ignores all changes, so the running check is
	-- replaced with the given answer.
	const function createInstanceMap(reports, gameRunning)
		const instanceMap = InstanceMap.new(function(instance, propertyName)
			if propertyName == "Parent" then
				table.insert(reports, instance)
			end
		end)
		instanceMap.__isGameRunning = function()
			return gameRunning
		end
		return instanceMap
	end

	-- Builds container > ancestor > leaf and tracks the ancestor and leaf.
	const function createTree(leafClassName, gameRunning)
		const reports = {}
		const instanceMap = createInstanceMap(reports, gameRunning == true)

		const container = Instance.new("Folder")
		const other = Instance.new("Folder")
		other.Parent = container

		const ancestor = Instance.new("Folder")
		ancestor.Name = "Ancestor"
		ancestor.Parent = container

		const leaf = Instance.new(leafClassName)
		leaf.Name = "Leaf"
		leaf.Parent = ancestor

		instanceMap:insert("ANCESTOR", ancestor)
		instanceMap:insert("LEAF", leaf)

		return {
			Ancestor = ancestor,
			Container = container,
			InstanceMap = instanceMap,
			Leaf = leaf,
			Other = other,
			Reports = reports,
		}
	end

	const function countReports(reports, instance)
		local count = 0
		for _, reported in reports do
			if reported == instance then
				count += 1
			end
		end
		return count
	end

	-- Signals can be deferred, so give them a frame to arrive before checking.
	const function settle()
		task.wait()
	end

	it("should retain authoritative parents until a paused move replaces them", function()
		const tree = createTree("Folder")
		const map = tree.InstanceMap
		expect(map.knownParents[tree.Leaf]).to.equal(tree.Ancestor)
		tree.Leaf.Parent = tree.Other
		settle()
		expect(map.knownParents[tree.Leaf]).to.equal(tree.Ancestor)
		map:pauseInstance(tree.Leaf)
		tree.Leaf.Parent = tree.Container
		settle()
		expect(map.knownParents[tree.Leaf]).to.equal(tree.Container)
		map:removeInstance(tree.Leaf)
		expect(map.knownParents[tree.Leaf]).to.equal(nil)
		expect(map.observedParents[tree.Leaf]).to.equal(nil)
		expect(map.pausedUpdateInstances[tree.Leaf]).to.equal(nil)
		map:stop()
		expect(next(map.knownParents)).to.equal(nil)
		tree.Container:Destroy()
	end)

	it("should report a leaf's Parent only when its own parent changes", function()
		for _, className in CLASS_NAMES do
			for _, action in ACTIONS do
				const tree = createTree(className)

				action.Run(tree)
				settle()

				const leafReports = countReports(tree.Reports, tree.Leaf)
				const context = `{className} after "{action.Name}"`
				if action.OwnParent then
					assert(leafReports > 0, `Expected the leaf's Parent change to be reported for {context}`)
				else
					assert(leafReports == 0, `Expected no Parent change for the leaf for {context}`)
				end

				tree.InstanceMap:stop()
				tree.Container:Destroy()
			end
		end
	end)

	it("should report a moved ancestor itself", function()
		for _, action in ACTIONS do
			if action.OwnParent then
				continue
			end

			const tree = createTree("Folder")

			action.Run(tree)
			settle()

			expect(countReports(tree.Reports, tree.Ancestor) > 0).to.equal(true)

			tree.InstanceMap:stop()
			tree.Container:Destroy()
		end
	end)

	it("should not report nested instances when a new subtree is placed", function()
		-- Mirrors how the reconciler builds new instances. Children are parented and
		-- tracked before their own parent is placed into the tree.
		const reports = {}
		const instanceMap = createInstanceMap(reports, false)

		const container = Instance.new("Folder")
		const top = Instance.new("Folder")
		const middle = Instance.new("Folder")
		const leaf = Instance.new("StringValue")

		leaf.Parent = middle
		instanceMap:insert("LEAF", leaf)
		middle.Parent = top
		instanceMap:insert("MIDDLE", middle)
		top.Parent = container
		instanceMap:insert("TOP", top)
		settle()

		expect(#reports).to.equal(0)

		instanceMap:stop()
		container:Destroy()
	end)

	it("should report nothing while the game is running", function()
		for _, action in ACTIONS do
			const tree = createTree("Folder", true)

			action.Run(tree)
			settle()

			expect(#tree.Reports).to.equal(0)

			tree.InstanceMap:stop()
			tree.Container:Destroy()
		end
	end)

	-- Every combination of the child being known, the parent being paused, and
	-- the game running. Only an unknown child of an active parent is reported.
	it("should report only unknown children added to a known parent", function()
		for _, known in { false, true } do
			for _, paused in { false, true } do
				for _, gameRunning in { false, true } do
					const added = {}
					const instanceMap = InstanceMap.new(nil, function(child)
						table.insert(added, child)
					end)
					instanceMap.__isGameRunning = function()
						return gameRunning
					end
					const parent = Instance.new("Folder")
					instanceMap:insert("PARENT", parent)
					if paused then
						instanceMap:pauseInstance(parent)
					end
					const child = Instance.new("Folder")
					if known then
						instanceMap:insert("CHILD", child)
					end

					child.Parent = parent
					settle()

					const expected = not known and not paused and not gameRunning
					expect(#added).to.equal(if expected then 1 else 0)
					instanceMap:stop()
					parent:Destroy()
				end
			end
		end
	end)

	it("should not report children of an untracked parent", function()
		const added = {}
		const instanceMap = InstanceMap.new(nil, function(child)
			table.insert(added, child)
		end)
		instanceMap.__isGameRunning = function()
			return false
		end
		const parent = Instance.new("Folder")
		instanceMap:insert("PARENT", parent)
		instanceMap:removeId("PARENT")
		Instance.new("Folder").Parent = parent
		settle()
		expect(#added).to.equal(0)
		instanceMap:stop()
		parent:Destroy()
	end)
end
