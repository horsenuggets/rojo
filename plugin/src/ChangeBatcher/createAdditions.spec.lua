return function()
	local InstanceMap = require(script.Parent.Parent.InstanceMap)
	local createAdditions = require(script.Parent.createAdditions)

	-- Records each copy that would be serialized, by describing its tree.
	local function describeTree(instance)
		local parts = { instance.Name }
		for _, child in instance:GetChildren() do
			table.insert(parts, describeTree(child))
		end
		return "(" .. table.concat(parts, " ") .. ")"
	end

	local function harness()
		local root = Instance.new("Folder")
		root.Name = "Root"
		local instanceMap = InstanceMap.new()
		instanceMap:insert("ROOT", root)
		local serialized = {}
		local counter = 0
		local function serialize(copy)
			table.insert(serialized, describeTree(copy))
			return "MODEL" .. #serialized
		end
		local function makeId()
			counter += 1
			return "ID" .. counter
		end
		local function run(candidates)
			local set = {}
			for _, candidate in candidates do
				set[candidate] = true
			end
			return createAdditions(instanceMap, set, serialize, makeId)
		end
		return root, instanceMap, serialized, run
	end

	local function folder(name, parent)
		local instance = Instance.new("Folder")
		instance.Name = name
		instance.Parent = parent
		return instance
	end

	it("should send a new subtree with an id for every instance", function()
		local root, instanceMap, serialized, run = harness()
		local created = folder("Created", root)
		local child = folder("Child", created)
		folder("Grandchild", child)

		local additions = run({ created })
		local addition = additions.ID1
		expect(addition.parent).to.equal("ROOT")
		expect(addition.model).to.equal("MODEL1")
		expect(serialized[1]).to.equal("(Created (Child (Grandchild)))")
		expect(addition.ids.name).to.equal("Created")
		expect(addition.ids.className).to.equal("Folder")
		expect(addition.ids.children[1].id).to.equal("ID2")
		expect(addition.ids.children[1].children[1].id).to.equal("ID3")
		-- Every new instance is known once it's sent.
		expect(instanceMap.fromIds.ID1).to.equal(created)
		expect(instanceMap.fromIds.ID2).to.equal(child)
		expect(instanceMap.fromIds.ID3).to.equal(child.Grandchild)
		instanceMap:stop()
		root:Destroy()
	end)

	-- Every combination of a candidate being removed again, moved under
	-- another new instance, or already known, and of it being archivable.
	it("should skip candidates that aren't new roots under a known parent", function()
		for _, state in { "kept", "removed", "nested", "known" } do
			for _, archivable in { true, false } do
				local root, instanceMap, _, run = harness()
				local other = folder("Other", root)
				local candidate = folder("Candidate", root)
				candidate.Archivable = archivable
				if state == "removed" then
					candidate.Parent = nil
				elseif state == "nested" then
					candidate.Parent = other
				elseif state == "known" then
					instanceMap:insert("KNOWN", candidate)
				end

				local additions = run({ candidate, other })
				local sentCandidate = false
				for _, addition in additions do
					if addition.ids.name == "Candidate" then
						sentCandidate = true
					end
				end
				expect(sentCandidate).to.equal(state == "kept" and archivable)
				instanceMap:stop()
				root:Destroy()
			end
		end
	end)

	-- An existing instance moved into a new one is sent as a move, so its
	-- copy must not be in the model too.
	it("should leave known descendants out of the model and the ids", function()
		local root, instanceMap, serialized, run = harness()
		local existing = folder("Existing", root)
		instanceMap:insert("EXISTING", existing)
		folder("ExistingChild", existing)
		local created = folder("Created", root)
		folder("Before", created)
		existing.Parent = created
		folder("After", created)

		local additions = run({ created })
		expect(serialized[1]).to.equal("(Created (Before) (After))")
		local names = {}
		for _, child in additions.ID1.ids.children do
			table.insert(names, child.name)
		end
		expect(table.concat(names, ",")).to.equal("Before,After")
		expect(instanceMap.fromIds.EXISTING).to.equal(existing)
		instanceMap:stop()
		root:Destroy()
	end)

	it("should leave instances that aren't archivable out of the ids", function()
		local root, instanceMap, serialized, run = harness()
		local created = folder("Created", root)
		folder("Saved", created)
		folder("Unsaved", created).Archivable = false

		local additions = run({ created })
		expect(#additions.ID1.ids.children).to.equal(1)
		expect(additions.ID1.ids.children[1].name).to.equal("Saved")
		expect(serialized[1]).to.equal("(Created (Saved))")
		instanceMap:stop()
		root:Destroy()
	end)

	-- Studio creates services like ControllerService on demand. Their Clone returns
	-- nil, so they must be skipped quietly instead of warning on every flush.
	it("should skip instances whose Clone returns nil, like services", function()
		local root, instanceMap, serialized, run = harness()
		local cloneable = folder("Cloneable", root)
		-- Stands in for a service, the only reachable case, by returning nil from Clone.
		local uncloneable = folder("Uncloneable", root)
		local stand = setmetatable({
			Clone = function()
				return nil
			end,
		}, {
			__index = function(_, key)
				return uncloneable[key]
			end,
		})

		local additions = run({ stand, cloneable })

		local names = {}
		for _, addition in additions do
			table.insert(names, addition.ids.name)
		end
		expect(table.concat(names, ",")).to.equal("Cloneable")
		expect(table.concat(serialized, ",")).to.equal("(Cloneable)")
		expect(instanceMap.fromInstances[stand]).to.equal(nil)
		instanceMap:stop()
		root:Destroy()
	end)

	it("should not track or send anything that fails to serialize", function()
		local root = Instance.new("Folder")
		local instanceMap = InstanceMap.new()
		instanceMap:insert("ROOT", root)
		local created = folder("Created", root)
		local additions = createAdditions(instanceMap, { [created] = true }, function()
			error("cannot serialize")
		end)
		expect(next(additions)).to.equal(nil)
		expect(instanceMap.fromInstances[created]).to.equal(nil)
		instanceMap:stop()
		root:Destroy()
	end)

	-- Serializing yields. Whatever happens to the subtree meanwhile, the ids
	-- describe what was serialized, and the new instances are already tracked.
	it("should describe the serialized subtree even if it changes while serializing", function()
		for _, change in { "rename", "add", "remove" } do
			local root = Instance.new("Folder")
			local instanceMap = InstanceMap.new()
			instanceMap:insert("ROOT", root)
			local created = folder("Created", root)
			local child = folder("Child", created)
			local trackedWhileSerializing = false
			local additions = createAdditions(instanceMap, { [created] = true }, function(copy)
				trackedWhileSerializing = instanceMap.fromInstances[child] ~= nil
				if change == "rename" then
					child.Name = "Renamed"
				elseif change == "add" then
					folder("Late", created)
				else
					child.Parent = nil
				end
				return describeTree(copy)
			end)
			local _, addition = next(additions)
			expect(addition.model).to.equal("(Created (Child))")
			expect(#addition.ids.children).to.equal(1)
			expect(addition.ids.children[1].name).to.equal("Child")
			expect(trackedWhileSerializing).to.equal(true)
			instanceMap:stop()
			root:Destroy()
		end
	end)

	-- A model only holds references between its own instances. Every
	-- combination of a reference that's unset, points inside the new subtree,
	-- or points at a known instance outside it.
	it("should return the references the model can't hold", function()
		for _, target in { "none", "inside", "outside" } do
			local root, instanceMap, _, run = harness()
			local known = Instance.new("Part")
			known.Parent = root
			instanceMap:insert("KNOWN", known)
			local created = folder("Created", root)
			local inner = Instance.new("Part")
			inner.Parent = created
			local pointer = Instance.new("ObjectValue")
			pointer.Parent = created
			if target == "inside" then
				pointer.Value = inner
			elseif target == "outside" then
				pointer.Value = known
			end

			local _, references = run({ created })
			if target == "outside" then
				expect(#references).to.equal(1)
				expect(references[1][1]).to.equal(pointer)
				expect(references[1][2]).to.equal("Value")
			else
				expect(#references).to.equal(0)
			end
			instanceMap:stop()
			root:Destroy()
		end
	end)

	it("should empty the candidate set", function()
		local root, instanceMap, _, _ = harness()
		local candidates = { [folder("Created", root)] = true }
		createAdditions(instanceMap, candidates, function()
			return "MODEL"
		end)
		expect(next(candidates)).to.equal(nil)
		instanceMap:stop()
		root:Destroy()
	end)
end
