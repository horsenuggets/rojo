return function()
	local hydrate = require(script.Parent.hydrate)

	local InstanceMap = require(script.Parent.Parent.InstanceMap)

	it("should match the root instance no matter what", function()
		local knownInstances = InstanceMap.new()
		local virtualInstances = {
			ROOT = {
				ClassName = "Model",
				Name = "Foo",
				Properties = {},
				Children = {},
			},
		}

		local rootInstance = Instance.new("Folder")

		hydrate(knownInstances, virtualInstances, "ROOT", rootInstance)

		expect(knownInstances:size()).to.equal(1)
		expect(knownInstances.fromIds["ROOT"]).to.equal(rootInstance)
	end)

	it("should not match children with mismatched ClassName", function()
		local knownInstances = InstanceMap.new()
		local virtualInstances = {
			ROOT = {
				ClassName = "Folder",
				Name = "Root",
				Properties = {},
				Children = {},
			},

			CHILD = {
				ClassName = "Folder",
				Name = "Child",
				Properties = {},
				Children = {},
			},
		}

		local rootInstance = Instance.new("Folder")

		-- ClassName of this instance is intentionally different
		local child = Instance.new("Model")
		child.Name = "Child"
		child.Parent = rootInstance

		hydrate(knownInstances, virtualInstances, "ROOT", rootInstance)

		expect(knownInstances:size()).to.equal(1)
		expect(knownInstances.fromIds["ROOT"]).to.equal(rootInstance)
	end)

	it("should not match children with mismatched Name", function()
		local knownInstances = InstanceMap.new()
		local virtualInstances = {
			ROOT = {
				ClassName = "Folder",
				Name = "Root",
				Properties = {},
				Children = {},
			},

			CHILD = {
				ClassName = "Folder",
				Name = "Child",
				Properties = {},
				Children = {},
			},
		}

		local rootInstance = Instance.new("Folder")

		-- Name of this instance is intentionally different
		local child = Instance.new("Folder")
		child.Name = "Not Child"
		child.Parent = rootInstance

		hydrate(knownInstances, virtualInstances, "ROOT", rootInstance)

		expect(knownInstances:size()).to.equal(1)
		expect(knownInstances.fromIds["ROOT"]).to.equal(rootInstance)
	end)

	it("should pair instances with matching Name and ClassName", function()
		local knownInstances = InstanceMap.new()
		local virtualInstances = {
			ROOT = {
				ClassName = "Folder",
				Name = "Root",
				Properties = {},
				Children = { "CHILD1", "CHILD2" },
			},

			CHILD1 = {
				ClassName = "Folder",
				Name = "Child 1",
				Properties = {},
				Children = {},
			},

			CHILD2 = {
				ClassName = "Model",
				Name = "Child 2",
				Properties = {},
				Children = {},
			},
		}

		local rootInstance = Instance.new("Folder")

		local child1 = Instance.new("Folder")
		child1.Name = "Child 1"
		child1.Parent = rootInstance

		local child2 = Instance.new("Model")
		child2.Name = "Child 2"
		child2.Parent = rootInstance

		hydrate(knownInstances, virtualInstances, "ROOT", rootInstance)

		expect(knownInstances:size()).to.equal(3)
		expect(knownInstances.fromIds["ROOT"]).to.equal(rootInstance)
		expect(knownInstances.fromIds["CHILD1"]).to.equal(child1)
		expect(knownInstances.fromIds["CHILD2"]).to.equal(child2)
	end)

	it("should disambiguate duplicate-named siblings by matching properties", function()
		local knownInstances = InstanceMap.new()
		local virtualInstances = {
			ROOT = {
				ClassName = "Folder",
				Name = "Root",
				Properties = {},
				Children = { "CHILD_A", "CHILD_B" },
			},

			CHILD_A = {
				ClassName = "StringValue",
				Name = "a",
				Properties = { Value = { String = "first" } },
				Children = {},
			},

			CHILD_B = {
				ClassName = "StringValue",
				Name = "a",
				Properties = { Value = { String = "second" } },
				Children = {},
			},
		}

		local rootInstance = Instance.new("Folder")

		-- Created in the reverse order of the virtual children, so a purely
		-- order-based tiebreak would mis-pair them.
		local child1 = Instance.new("StringValue")
		child1.Name = "a"
		child1.Value = "second"
		child1.Parent = rootInstance

		local child2 = Instance.new("StringValue")
		child2.Name = "a"
		child2.Value = "first"
		child2.Parent = rootInstance

		hydrate(knownInstances, virtualInstances, "ROOT", rootInstance)

		expect(knownInstances:size()).to.equal(3)
		expect(knownInstances.fromIds["CHILD_A"]).to.equal(child2)
		expect(knownInstances.fromIds["CHILD_B"]).to.equal(child1)
	end)

	it("should fall back to child order for duplicate-named siblings with no distinguishing properties", function()
		local knownInstances = InstanceMap.new()
		local virtualInstances = {
			ROOT = {
				ClassName = "Folder",
				Name = "Root",
				Properties = {},
				Children = { "CHILD_A", "CHILD_B" },
			},

			CHILD_A = {
				ClassName = "Folder",
				Name = "a",
				Properties = {},
				Children = {},
			},

			CHILD_B = {
				ClassName = "Folder",
				Name = "a",
				Properties = {},
				Children = {},
			},
		}

		local rootInstance = Instance.new("Folder")

		local child1 = Instance.new("Folder")
		child1.Name = "a"
		child1.Parent = rootInstance

		local child2 = Instance.new("Folder")
		child2.Name = "a"
		child2.Parent = rootInstance

		hydrate(knownInstances, virtualInstances, "ROOT", rootInstance)

		expect(knownInstances:size()).to.equal(3)
		-- With equal scores the earliest unvisited child wins, preserving the
		-- original order-based behavior.
		expect(knownInstances.fromIds["CHILD_A"]).to.equal(child1)
		expect(knownInstances.fromIds["CHILD_B"]).to.equal(child2)
	end)

	it("should fall back to child order for very large duplicate-named groups", function()
		-- More candidates than hydrate is willing to score at once. The group
		-- must fall back to order-based matching, so virtual child N pairs with
		-- existing child N regardless of properties.
		local count = 64

		local knownInstances = InstanceMap.new()
		local virtualInstances = {
			ROOT = {
				ClassName = "Folder",
				Name = "Root",
				Properties = {},
				Children = {},
			},
		}

		local rootInstance = Instance.new("Folder")

		local expectedInstances = {}
		for i = 1, count do
			local id = "CHILD_" .. i
			table.insert(virtualInstances.ROOT.Children, id)
			virtualInstances[id] = {
				ClassName = "StringValue",
				Name = "a",
				-- Distinct values that, if scored, would pair by value rather
				-- than by order.
				Properties = { Value = { String = "value " .. i } },
				Children = {},
			}

			local child = Instance.new("StringValue")
			child.Name = "a"
			child.Value = "value " .. (count - i + 1)
			child.Parent = rootInstance
			expectedInstances[id] = child
		end

		hydrate(knownInstances, virtualInstances, "ROOT", rootInstance)

		expect(knownInstances:size()).to.equal(count + 1)
		for id, expectedInstance in expectedInstances do
			expect(knownInstances.fromIds[id]).to.equal(expectedInstance)
		end
	end)

	-- A tree of a root, `width` children, and one grandchild under each.
	local function tree(width)
		local virtualInstances = {
			ROOT = { ClassName = "Folder", Name = "Root", Properties = {}, Children = {} },
		}
		local rootInstance = Instance.new("Folder")
		for index = 1, width do
			local childId = "CHILD_" .. index
			local leafId = "LEAF_" .. index
			virtualInstances[childId] =
				{ ClassName = "Folder", Name = "Child" .. index, Properties = {}, Children = { leafId } }
			virtualInstances[leafId] = { ClassName = "Folder", Name = "Leaf", Properties = {}, Children = {} }
			table.insert(virtualInstances.ROOT.Children, childId)

			local child = Instance.new("Folder")
			child.Name = "Child" .. index
			child.Parent = rootInstance
			local leaf = Instance.new("Folder")
			leaf.Name = "Leaf"
			leaf.Parent = child
		end
		return virtualInstances, rootInstance
	end

	-- The caller yields from `step`, so a walk that skipped it for part of the
	-- tree could still outlast Studio's script timeout.
	it("should call step at least once for every hydrated instance", function()
		for _, width in { 0, 1, 7 } do
			local virtualInstances, rootInstance = tree(width)
			local knownInstances = InstanceMap.new()
			local steps = 0
			hydrate(knownInstances, virtualInstances, "ROOT", rootInstance, function()
				steps += 1
			end)
			expect(knownInstances:size()).to.equal(1 + width * 2)
			expect(steps >= knownInstances:size()).to.equal(true)
			knownInstances:stop()
			rootInstance:Destroy()
		end
	end)

	-- Children that match nothing are never recursed into, on either side, so
	-- a wide folder of them must still give the caller chances to yield.
	it("should call step for children that match nothing", function()
		for _, case in { { 40, 0 }, { 0, 40 }, { 25, 30 } } do
			local existingCount, virtualCount = case[1], case[2]
			local virtualInstances = {
				ROOT = { ClassName = "Folder", Name = "Root", Properties = {}, Children = {} },
			}
			local rootInstance = Instance.new("Folder")
			for index = 1, existingCount do
				local child = Instance.new("Folder")
				child.Name = "OnlyInStudio" .. index
				child.Parent = rootInstance
			end
			for index = 1, virtualCount do
				local id = "VIRTUAL_" .. index
				virtualInstances[id] =
					{ ClassName = "Folder", Name = "OnlyOnDisk" .. index, Properties = {}, Children = {} }
				table.insert(virtualInstances.ROOT.Children, id)
			end

			local knownInstances = InstanceMap.new()
			local steps = 0
			hydrate(knownInstances, virtualInstances, "ROOT", rootInstance, function()
				steps += 1
			end)
			-- Only the root matched.
			expect(knownInstances:size()).to.equal(1)
			expect(steps >= existingCount + virtualCount).to.equal(true)
			knownInstances:stop()
			rootInstance:Destroy()
		end
	end)

	-- A caller stops the walk by raising from step, and nothing may be
	-- hydrated after that.
	it("should stop hydrating when step raises", function()
		local virtualInstances, rootInstance = tree(6)
		local knownInstances = InstanceMap.new()
		local steps = 0
		local ok, err = pcall(hydrate, knownInstances, virtualInstances, "ROOT", rootInstance, function()
			steps += 1
			if steps == 10 then
				error("stop", 0)
			end
		end)
		expect(ok).to.equal(false)
		expect(err).to.equal("stop")
		expect(steps).to.equal(10)
		local sizeAtStop = knownInstances:size()
		expect(sizeAtStop < 13).to.equal(true)
		expect(knownInstances:size()).to.equal(sizeAtStop)
		knownInstances:stop()
		rootInstance:Destroy()
	end)

	it("should hydrate the same instances when step yields", function()
		local virtualInstances, rootInstance = tree(5)
		local plain = InstanceMap.new()
		hydrate(plain, virtualInstances, "ROOT", rootInstance)

		local yielding = InstanceMap.new()
		hydrate(yielding, virtualInstances, "ROOT", rootInstance, function()
			task.wait()
		end)

		expect(yielding:size()).to.equal(plain:size())
		for id, instance in plain.fromIds do
			expect(yielding.fromIds[id]).to.equal(instance)
		end
		plain:stop()
		yielding:stop()
		rootInstance:Destroy()
	end)
end
