return function()
	local Plugin = script.Parent
	local InstanceMap = require(Plugin.InstanceMap)
	local PatchSet = require(Plugin.PatchSet)
	local PatchTree = require(Plugin.PatchTree)
	local Timer = require(Plugin.Timer)
	local createSourceSetter = require(Plugin.Reconciler.setSource)

	local maps, instances
	local environment, originalOs, originalTask

	beforeEach(function()
		maps, instances = {}, {}
		environment = getfenv(PatchTree.build)
		originalOs, originalTask = environment.os, environment.task
	end)

	afterEach(function()
		environment.os, environment.task = originalOs, originalTask
		for _, map in maps do
			map:stop()
		end
		for _, instance in instances do
			instance:Destroy()
		end
	end)

	-- All functions in the module share this environment, including the private yield
	-- helper. Override only its clock and scheduler, never copy protected native globals.
	local function controlClock(boundary)
		local time, waits = 0, 0
		environment.os = setmetatable({
			clock = function()
				time += 0.1
				return time
			end,
		}, { __index = originalOs })
		environment.task = setmetatable({
			wait = function()
				waits += 1
				local entries = Timer._entries[coroutine.running()]
				for _, entry in entries or {} do
					expect(entry.Profiled).to.equal(false)
				end
				if waits == boundary then
					coroutine.yield("building")
				end
			end,
		}, { __index = originalTask })
	end

	local function resume(thread)
		local success, result = coroutine.resume(thread)
		assert(success, result)
		return result
	end

	local function fixture()
		local source = createSourceSetter({
			ConnectChanged = function()
				return { Disconnect = function() end }
			end,
			GetEditorSource = function()
				return "return true"
			end,
			Spawn = function()
				error("No Source writes should be scheduled.")
			end,
			UpdateSourceAsync = function()
				error("No Source writes should be invoked.")
			end,
		})
		local function newMap()
			local map = InstanceMap.new(nil, nil, source)
			table.insert(maps, map)
			return map
		end
		local parent = Instance.new("Folder")
		parent.Name = "Unknown ancestor"
		table.insert(instances, parent)
		local leaf = Instance.new("ModuleScript")
		leaf.Name = "Removed script"
		leaf.Parent = parent
		local old = newMap()
		old:insert("SCRIPT", leaf)
		return old, newMap, parent, leaf
	end

	for _, phase in { "updated", "removed", "added" } do
		for _, boundary in { 1, 2 } do
			it(
				`should retire the real {phase} walk at yield {boundary} without stealing a replacement lease`,
				function()
					local old, newMap, parent, leaf = fixture()
					local patch = PatchSet.newEmpty()
					if phase == "updated" then
						old:insert("PARENT", parent)
						table.insert(patch.updated, { id = "SCRIPT", changedProperties = {}, changedName = "Changed" })
					elseif phase == "added" then
						old:insert("PARENT", parent)
						patch.added.NEW = {
							Id = "NEW",
							Parent = "PARENT",
							ClassName = "Folder",
							Name = "Added",
							Properties = {},
						}
					end
					-- Even after an updated/ancestry yield, a removed Instance can reinsert itself.
					table.insert(patch.removed, leaf)
					if phase == "added" then
						table.clear(patch.removed)
					end
					local current = true
					controlClock(boundary)
					local result
					local thread = coroutine.create(function()
						result = PatchTree.build(patch, old, nil, function()
							return current
						end)
					end)
					expect(resume(thread)).to.equal("building")
					current = false
					old:stop()
					local replacement = newMap()
					replacement:insert("NEW_SCRIPT", leaf)
					local lease = replacement.sourceConnections[leaf]
					resume(thread)
					expect(coroutine.status(thread)).to.equal("dead")
					expect(old:size()).to.equal(0)
					expect(next(old.instancesToSignal)).to.equal(nil)
					expect(next(old.sourceConnections)).to.equal(nil)
					expect(replacement.sourceConnections[leaf]).to.equal(lease)
					local revision, generation = replacement:getSourceRevision(leaf)
					expect(revision).to.equal(0)
					expect(generation).to.equal(lease.Generation)
					expect(result).to.equal(nil)
					expect(Timer._entries[thread]).to.equal(nil)
				end
			)
		end
	end

	for _, target in { "ancestor", "script" } do
		it(`should recheck ownership immediately before inserting the removed {target}`, function()
			local old, newMap, parent, leaf = fixture()
			old:stop()
			if target == "script" then
				leaf.Parent = nil
				table.insert(instances, leaf)
			end
			local replacement = newMap()
			replacement:insert("NEW_SCRIPT", leaf)
			local lease = replacement.sourceConnections[leaf]
			local current = true
			setmetatable(old.fromInstances, {
				__index = function(_, instance)
					if instance == (if target == "ancestor" then parent else leaf) then
						current = false
					end
					return nil
				end,
			})
			local patch = PatchSet.newEmpty()
			table.insert(patch.removed, leaf)
			local result = PatchTree.build(patch, old, nil, function()
				return current
			end)
			expect(old:size()).to.equal(0)
			expect(replacement:getSourceRevision(leaf)).to.equal(0)
			expect(replacement.sourceConnections[leaf]).to.equal(lease)
			expect(result).to.equal(nil)
			expect(Timer._entries[coroutine.running()]).to.equal(nil)
		end)
	end

	for _, guarded in { false, true } do
		it(
			`should preserve a healthy {if guarded then "guarded" else "three-argument"} build through yields`,
			function()
				local map, _, parent, leaf = fixture()
				local patch = PatchSet.newEmpty()
				table.insert(patch.removed, leaf)
				patch.added.NEW = { Id = "NEW", ClassName = "Folder", Name = "Added", Properties = {} }
				local headers = { "Property", "Old", "New" }
				controlClock(1)
				local tree
				local thread = coroutine.create(function()
					if guarded then
						tree = PatchTree.build(patch, map, headers, function()
							return true
						end)
					else
						tree = PatchTree.build(patch, map, headers)
					end
				end)
				expect(resume(thread)).to.equal("building")
				resume(thread)
				expect(tree:getCount()).to.equal(3)
				expect(tree:getNode("SCRIPT").patchType).to.equal("Remove")
				expect(tree:getNode("NEW").patchType).to.equal("Add")
				expect(map.fromInstances[parent] ~= nil).to.equal(true)
				expect(map:getSourceRevision(leaf)).to.equal(0)
				expect(Timer._entries[thread]).to.equal(nil)
			end
		)
	end

	it("should preserve edit headers and read-only patch data for a current build", function()
		local map, _, parent = fixture()
		map:insert("PARENT", parent)
		local change = table.freeze({
			id = "SCRIPT",
			changedName = "Incoming name",
			changedProperties = table.freeze({}),
		})
		local patch = table.freeze({
			added = table.freeze({}),
			removed = table.freeze({}),
			updated = table.freeze({ change }),
		})
		local headers = table.freeze({ "Property", "Current", "Incoming" })
		local tree = PatchTree.build(patch, map, headers, function()
			return true
		end)
		local node = tree:getNode("SCRIPT")
		expect(node.patchType).to.equal("Edit")
		expect(node.changeInfo.edits).to.equal(1)
		expect(node.changeList[1]).to.equal(headers)
		expect(node.changeList[2][2]).to.equal("Removed script")
		expect(node.changeList[2][3]).to.equal("Incoming name")
		expect(patch.updated[1]).to.equal(change)
		expect(Timer._entries[coroutine.running()]).to.equal(nil)
	end)

	it("should reject a retired empty build and preserve a current empty build", function()
		local map = fixture()
		local patch = PatchSet.newEmpty()
		expect(PatchTree.build(patch, map, nil, function()
			return false
		end)).to.equal(nil)
		expect(PatchTree.build(patch, map):getCount()).to.equal(0)
		expect(Timer._entries[coroutine.running()]).to.equal(nil)
	end)
end
