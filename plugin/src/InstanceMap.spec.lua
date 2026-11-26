--[[

InstanceMap.spec

Tests structural change reporting and attribute ownership. Only an instance whose own
Parent changed should be reported, never the descendants of an instance that moved.

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

	-- Keep observations fixed while actual values change, modelling queued native signals.
	const function withholdSignals(map, instance)
		for _, connection in map.instancesToSignal[instance] do
			connection:Disconnect()
		end
	end

	-- Signals can be deferred, so give them a frame to arrive before checking.
	const function settle()
		task.wait()
	end

	for _, className in CLASS_NAMES do
		it(`does not report unmanaged attribute changes on {className}`, function()
			const reports = {}
			const map = InstanceMap.new(function(_, property)
				if property == "Attributes" then
					table.insert(reports, property)
				end
			end)
			map.__isGameRunning = function()
				return false
			end
			const instance = Instance.new(className)
			map:insert("ATTRIBUTES", instance)
			const ok, failure = pcall(function()
				for _, name in
					{
						"Rojo_Id",
						"Rojo_Target_Link",
						"__RojoPlus_ConnectionUrl",
						"__RojoPlus_LiveEditBootstrapped",
					}
				do
					instance:SetAttribute(name, "owned")
					settle()
					expect(instance:GetAttribute(name)).to.equal("owned")
					expect(#reports).to.equal(0)
					instance:SetAttribute(name, nil)
					settle()
					expect(#reports).to.equal(0)
				end
				instance:SetAttribute("Authored", "kept")
				settle()
				expect(#reports).to.equal(1)
				table.clear(reports)
				instance:SetAttribute("__RojoPlus_ConnectionUrl", "localhost:34872")
				instance:SetAttribute("Authored", "changed")
				settle()
				expect(#reports).to.equal(1)
			end)
			map:stop()
			instance:Destroy()
			assert(ok, failure)
		end)
	end

	it("should capture undelivered structural changes once without advancing authority", function()
		for _, className in CLASS_NAMES do
			const tree = createTree(className)
			const map = tree.InstanceMap
			const reports = {}
			withholdSignals(map, tree.Leaf)
			map.onInstanceChanged = function(instance, property)
				table.insert(reports, { Property = property, Value = instance[property] })
			end
			tree.Leaf.Name = "Undelivered name"
			tree.Leaf.Parent = tree.Other
			map:pauseInstance(tree.Leaf)
			map:captureStructuralChanges(tree.Leaf)
			map:captureStructuralChanges(tree.Leaf)
			map:__maybeFireInstanceChanged(tree.Leaf, "Name")
			map:__maybeFireInstanceChanged(tree.Leaf, "Parent")
			expect(#reports).to.equal(2)
			expect(reports[1].Property).to.equal("Name")
			expect(reports[1].Value).to.equal("Undelivered name")
			expect(reports[2].Property).to.equal("Parent")
			expect(reports[2].Value).to.equal(tree.Other)
			expect(map.knownParents[tree.Leaf]).to.equal(tree.Ancestor)
			map:stop()
			tree.Container:Destroy()
		end
	end)

	it("should stop a structural capture when its notification reinserts the identity", function()
		const tree = createTree("Folder")
		const map = tree.InstanceMap
		const reports = {}
		withholdSignals(map, tree.Leaf)
		tree.Leaf.Name = "New lease"
		tree.Leaf.Parent = tree.Other
		map.onInstanceChanged = function(instance, property)
			table.insert(reports, property)
			map:insert("LEAF", instance)
		end
		map:captureStructuralChanges(tree.Leaf)
		expect(#reports).to.equal(1)
		expect(reports[1]).to.equal("Name")
		expect(map.knownParents[tree.Leaf]).to.equal(tree.Other)
		map:stop()
		tree.Container:Destroy()
	end)

	it("should consult the assignment boundary before replacing an unobserved value", function()
		const tree = createTree("Folder")
		const map = tree.InstanceMap
		withholdSignals(map, tree.Leaf)
		tree.Leaf.Name = "Undelivered name"
		map.onBeforeStructuralAssignment = function(instance, property, value)
			expect(instance[property]).to.equal("Undelivered name")
			expect(map.observedNames[instance]).to.equal("Leaf")
			expect(value).to.equal("Older echo")
			map:captureStructuralChanges(instance)
			error("The caller must revalidate this generation.")
		end
		const success = pcall(map.applyStructuralProperty, map, tree.Leaf, "Name", "Older echo")
		expect(success).to.equal(false)
		expect(tree.Leaf.Name).to.equal("Undelivered name")
		expect(map.observedNames[tree.Leaf]).to.equal("Undelivered name")
		expect(next(map.structuralAssignments)).to.equal(nil)
		map:stop()
		tree.Container:Destroy()
	end)

	it("should report local Parent and Name edits even while the instance is paused", function()
		for _, className in CLASS_NAMES do
			const tree = createTree(className)
			const map = tree.InstanceMap
			const reports = {}
			map.onInstanceChanged = function(instance, propertyName)
				if instance == tree.Leaf then
					reports[propertyName] = (reports[propertyName] or 0) + 1
				end
			end
			map:pauseInstance(tree.Leaf)
			tree.Leaf.Parent = tree.Other
			tree.Leaf.Name = "Local name"
			settle()
			expect(reports.Parent).to.equal(1)
			expect(reports.Name).to.equal(1)
			expect(map.knownParents[tree.Leaf]).to.equal(tree.Ancestor)
			map:stop()
			tree.Container:Destroy()
		end
	end)

	it("should report a same-frame local edit after an explicit remote structural assignment", function()
		for _, className in CLASS_NAMES do
			const tree = createTree(className)
			const map = tree.InstanceMap
			const reports = {}
			map.onInstanceChanged = function(instance, propertyName)
				if instance == tree.Leaf then
					reports[propertyName] = (reports[propertyName] or 0) + 1
				end
			end
			map:pauseInstance(tree.Leaf)
			expect(map:applyStructuralProperty(tree.Leaf, "Parent", tree.Other)).to.equal(true)
			expect(map:applyStructuralProperty(tree.Leaf, "Name", "Server name")).to.equal(true)
			tree.Leaf.Parent = tree.Container
			tree.Leaf.Name = "Local name"
			settle()
			expect(reports.Parent).to.equal(1)
			expect(reports.Name).to.equal(1)
			expect(map.knownParents[tree.Leaf]).to.equal(tree.Other)
			map:stop()
			tree.Container:Destroy()
		end
	end)

	it("should suppress only explicit remote Parent and Name effects", function()
		for _, className in CLASS_NAMES do
			const tree = createTree(className)
			const map = tree.InstanceMap
			const reports = {}
			map.onInstanceChanged = function(_, propertyName)
				table.insert(reports, propertyName)
			end
			expect(map:applyStructuralProperty(tree.Leaf, "Parent", tree.Other)).to.equal(true)
			expect(map:applyStructuralProperty(tree.Leaf, "Name", "Server name")).to.equal(true)
			map:unpauseAllInstances()
			settle()
			map:__maybeFireInstanceChanged(tree.Leaf, "Parent")
			map:__maybeFireInstanceChanged(tree.Leaf, "Name")
			expect(#reports).to.equal(0)
			expect(map.knownParents[tree.Leaf]).to.equal(tree.Other)
			expect(next(map.structuralAssignments)).to.equal(nil)
			map:stop()
			tree.Container:Destroy()
		end
	end)

	it("should retain reentrant local edits during immediate remote notifications", function()
		for _, propertyName in { "Parent", "Name" } do
			const tree = createTree("Folder")
			const map = tree.InstanceMap
			const reports = {}
			map.onInstanceChanged = function(instance, property)
				if instance == tree.Leaf and property == propertyName then
					table.insert(reports, instance[property])
				end
			end
			const remote = if propertyName == "Parent" then tree.Other else "Server name"
			const localValue = if propertyName == "Parent" then tree.Container else "Local name"
			map.__assignStructuralProperty = function(self, instance, property, value)
				InstanceMap.__assignStructuralProperty(self, instance, property, value)
				self:__maybeFireInstanceChanged(instance, property)
				instance[property] = localValue
				self:__maybeFireInstanceChanged(instance, property)
			end
			map:pauseInstance(tree.Leaf)
			expect(map:applyStructuralProperty(tree.Leaf, propertyName, remote)).to.equal(true)
			expect(#reports).to.equal(1)
			expect(reports[1]).to.equal(localValue)
			settle()
			expect(#reports).to.equal(1)
			expect(tree.Leaf[propertyName]).to.equal(localValue)
			expect(map.knownParents[tree.Leaf]).to.equal(if propertyName == "Parent" then remote else tree.Ancestor)
			map:stop()
			tree.Container:Destroy()
		end
	end)

	it("should keep a nested remote assignment newer than its returning predecessor", function()
		const tree = createTree("Folder")
		const map = tree.InstanceMap
		local nested = false
		map.__assignStructuralProperty = function(self, instance, propertyName, value)
			InstanceMap.__assignStructuralProperty(self, instance, propertyName, value)
			if not nested then
				nested = true
				expect(self:applyStructuralProperty(instance, propertyName, tree.Container)).to.equal(true)
			end
		end
		expect(map:applyStructuralProperty(tree.Leaf, "Parent", tree.Other)).to.equal(true)
		settle()
		expect(tree.Leaf.Parent).to.equal(tree.Container)
		expect(map.knownParents[tree.Leaf]).to.equal(tree.Container)
		expect(map.observedParents[tree.Leaf]).to.equal(tree.Container)
		expect(#tree.Reports).to.equal(0)
		expect(next(map.structuralAssignments)).to.equal(nil)
		map:stop()
		tree.Container:Destroy()
	end)

	it("should retain an outer accepted move when a nested assignment fails", function()
		const tree = createTree("Folder")
		const map = tree.InstanceMap
		local nested = false
		map.__assignStructuralProperty = function(self, instance, propertyName, value)
			InstanceMap.__assignStructuralProperty(self, instance, propertyName, value)
			if not nested then
				nested = true
				expect(self:applyStructuralProperty(instance, propertyName, instance)).to.equal(false)
			end
		end
		expect(map:applyStructuralProperty(tree.Leaf, "Parent", tree.Other)).to.equal(true)
		settle()
		expect(tree.Leaf.Parent).to.equal(tree.Other)
		expect(map.knownParents[tree.Leaf]).to.equal(tree.Other)
		expect(map.observedParents[tree.Leaf]).to.equal(tree.Other)
		expect(#tree.Reports).to.equal(0)
		expect(next(map.structuralAssignments)).to.equal(nil)
		map:stop()
		tree.Container:Destroy()
	end)

	it("should preserve a newer observation when assignment failure returns", function()
		const tree = createTree("Folder")
		const map = tree.InstanceMap
		map.__assignStructuralProperty = function(self, instance, propertyName)
			instance[propertyName] = tree.Container
			self:__maybeFireInstanceChanged(instance, propertyName)
			error("Injected assignment failure.", 0)
		end
		expect(map:applyStructuralProperty(tree.Leaf, "Parent", tree.Other)).to.equal(false)
		settle()
		expect(map.observedParents[tree.Leaf]).to.equal(tree.Container)
		expect(map.knownParents[tree.Leaf]).to.equal(tree.Ancestor)
		expect(countReports(tree.Reports, tree.Leaf)).to.equal(1)
		expect(next(map.structuralAssignments)).to.equal(nil)
		map:stop()
		tree.Container:Destroy()
	end)

	it("should not attach a returning assignment to a reinserted identity", function()
		const tree = createTree("Folder")
		const map = tree.InstanceMap
		map.__assignStructuralProperty = function(self, instance, propertyName, value)
			InstanceMap.__assignStructuralProperty(self, instance, propertyName, value)
			self:removeInstance(instance)
			instance.Parent = tree.Container
			self:insert("LEAF", instance)
		end
		expect(map:applyStructuralProperty(tree.Leaf, "Parent", tree.Other)).to.equal(true)
		settle()
		expect(map.knownParents[tree.Leaf]).to.equal(tree.Container)
		expect(map.observedParents[tree.Leaf]).to.equal(tree.Container)
		expect(next(map.structuralAssignments)).to.equal(nil)
		map:stop()
		tree.Container:Destroy()
	end)

	it("should roll back failed structural assignments without hiding the next local edit", function()
		const tree = createTree("Folder")
		const map = tree.InstanceMap
		expect(map:applyStructuralProperty(tree.Ancestor, "Parent", tree.Leaf)).to.equal(false)
		expect(tree.Ancestor.Parent).to.equal(tree.Container)
		expect(map.knownParents[tree.Ancestor]).to.equal(tree.Container)
		expect(map.observedParents[tree.Ancestor]).to.equal(tree.Container)
		expect(next(map.structuralAssignments)).to.equal(nil)
		map:pauseInstance(tree.Ancestor)
		tree.Ancestor.Parent = tree.Other
		settle()
		expect(countReports(tree.Reports, tree.Ancestor)).to.equal(1)
		expect(map.knownParents[tree.Ancestor]).to.equal(tree.Container)
		map:stop()
		tree.Container:Destroy()
	end)

	it("should suppress authoritative detach and clear structural observation state", function()
		const tree = createTree("Folder")
		const map = tree.InstanceMap
		map:destroyInstance(tree.Leaf)
		settle()
		expect(#tree.Reports).to.equal(0)
		expect(tree.Leaf.Parent).to.equal(nil)
		expect(map.knownParents[tree.Leaf]).to.equal(nil)
		expect(map.observedNames[tree.Leaf]).to.equal(nil)
		expect(map.observedParents[tree.Leaf]).to.equal(nil)
		expect(map.structuralAssignments[tree.Leaf]).to.equal(nil)
		map:stop()
		tree.Leaf:Destroy()
		tree.Container:Destroy()
	end)

	it("should retain authoritative parents until an explicit remote move replaces them", function()
		const tree = createTree("Folder")
		const map = tree.InstanceMap
		expect(map.knownParents[tree.Leaf]).to.equal(tree.Ancestor)
		tree.Leaf.Parent = tree.Other
		settle()
		expect(map.knownParents[tree.Leaf]).to.equal(tree.Ancestor)
		-- A pause alone no longer attributes arbitrary local changes to the server.
		map:pauseInstance(tree.Leaf)
		expect(map:applyStructuralProperty(tree.Leaf, "Parent", tree.Container)).to.equal(true)
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
