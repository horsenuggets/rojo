return function()
	local hydrate = require(script.Parent.hydrate)

	local InstanceMap = require(script.Parent.Parent.InstanceMap)
	local diff = require(script.Parent.diff)

	local function duplicateTree(order, ignoreUnknown, temporary, nonArchivableLeaf)
		local virtualInstances = {
			ROOT = { ClassName = "Folder", Name = "Root", Properties = {}, Children = { "A", "B" } },
			A = {
				ClassName = "Folder",
				Name = "Repeated",
				Properties = {},
				Children = { "LEAF" },
				Metadata = { ignoreUnknownInstances = ignoreUnknown },
			},
			B = {
				ClassName = "Folder",
				Name = "Repeated",
				Properties = {},
				Children = {},
				Metadata = { ignoreUnknownInstances = ignoreUnknown },
			},
			LEAF = { ClassName = "Folder", Name = "Leaf", Properties = {}, Children = {} },
		}
		local rootInstance = Instance.new("Folder")
		rootInstance.Name = "Root"
		local expected = { ROOT = rootInstance }
		local unknown = {}
		for _, id in order do
			local child = Instance.new("Folder")
			child.Name = "Repeated"
			child.Parent = rootInstance
			expected[id] = child
			if id == "A" then
				local leaf = Instance.new("Folder")
				leaf.Name = "Leaf"
				leaf.Archivable = not nonArchivableLeaf
				leaf.Parent = child
				expected.LEAF = leaf
			end
			if temporary then
				local extra = Instance.new(temporary.ClassName)
				extra.Name = temporary.Name
				extra.Archivable = temporary.Archivable
				extra.Parent = child
				table.insert(unknown, { Instance = extra, Parent = child })
			end
		end
		return virtualInstances, rootInstance, expected, unknown
	end

	local function expectUnchanged(virtualInstances, rootInstance, expected, unknown)
		local knownInstances = InstanceMap.new()
		local ok, failure = pcall(function()
			hydrate(knownInstances, virtualInstances, "ROOT", rootInstance)
			for id, instance in expected do
				expect(knownInstances.fromIds[id]).to.equal(instance)
			end
			local success, patch = diff(knownInstances, virtualInstances, "ROOT")
			expect(success).to.equal(true)
			expect(next(patch.added)).to.equal(nil)
			expect(#patch.removed).to.equal(0)
			expect(#patch.updated).to.equal(0)
			for _, extra in unknown do
				expect(extra.Instance.Parent).to.equal(extra.Parent)
				expect(knownInstances.fromInstances[extra.Instance]).to.equal(nil)
			end
		end)
		knownInstances:stop()
		rootInstance:Destroy()
		assert(ok, failure)
	end

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
		-- A better property match must win even when the other candidate has
		-- the exact child count expected by the virtual instance.
		local leaf = Instance.new("Folder")
		leaf.Parent = child2

		hydrate(knownInstances, virtualInstances, "ROOT", rootInstance)

		expect(knownInstances:size()).to.equal(3)
		expect(knownInstances.fromIds["CHILD_A"]).to.equal(child2)
		expect(knownInstances.fromIds["CHILD_B"]).to.equal(child1)
	end)

	it("should retain duplicate-named subtrees when native sibling order differs", function()
		for _, ignoreUnknown in { false, true } do
			local virtualInstances = {
				ROOT = { ClassName = "Folder", Name = "Root", Properties = {}, Children = {} },
			}
			local rootInstance = Instance.new("Folder")
			rootInstance.Name = "Root"
			local expected = { ROOT = rootInstance }
			for _, count in { 2, 1, 0 } do
				local id = "CHILD_" .. count
				local child = Instance.new("Folder")
				child.Name = "Repeated"
				child.Parent = rootInstance
				expected[id] = child
				virtualInstances[id] = {
					ClassName = "Folder",
					Name = "Repeated",
					Properties = {},
					Children = {},
					Metadata = { ignoreUnknownInstances = ignoreUnknown },
				}
				table.insert(virtualInstances.ROOT.Children, 1, id)
				for leafIndex = 1, count do
					local leafId = id .. "_" .. leafIndex
					local leaf = Instance.new("Folder")
					leaf.Name = "Leaf"
					leaf.Parent = child
					expected[leafId] = leaf
					table.insert(virtualInstances[id].Children, leafId)
					virtualInstances[leafId] = { ClassName = "Folder", Name = "Leaf", Properties = {}, Children = {} }
				end
			end

			local knownInstances = InstanceMap.new()
			local ok, failure = pcall(function()
				hydrate(knownInstances, virtualInstances, "ROOT", rootInstance, function()
					task.wait()
				end)
				for id, instance in expected do
					expect(knownInstances.fromIds[id]).to.equal(instance)
				end
				local diffSuccess, patch = require(script.Parent.diff)(knownInstances, virtualInstances, "ROOT")
				expect(diffSuccess).to.equal(true)
				expect(next(patch.added)).to.equal(nil)
				expect(#patch.removed).to.equal(0)
				expect(#patch.updated).to.equal(0)
			end)
			knownInstances:stop()
			rootInstance:Destroy()
			assert(ok, failure)
		end
	end)

	it("should distinguish equal-sized subtrees by child names, classes, and multiplicities", function()
		for _, definitions in
			{
				{
					{ { "A", "Folder" }, { "B", "Folder" }, { "C", "Folder" } },
					{ { "D", "Folder" }, { "E", "Folder" }, { "F", "Folder" } },
				},
				{
					{ { "Same", "Folder" }, { "Same", "StringValue" }, { "Same", "IntValue" } },
					{ { "Same", "Folder" }, { "Same", "StringValue" }, { "Same", "BoolValue" } },
				},
				{
					{ { "X", "Folder" }, { "X", "Folder" }, { "Y", "Folder" } },
					{ { "X", "Folder" }, { "Y", "Folder" }, { "Y", "Folder" } },
				},
			}
		do
			for _, ignoreUnknown in { false, true } do
				local virtualInstances = {
					ROOT = { ClassName = "Folder", Name = "Root", Properties = {}, Children = { "A", "B" } },
				}
				local rootInstance = Instance.new("Folder")
				rootInstance.Name = "Root"
				local expected = { ROOT = rootInstance }
				for _, index in { 2, 1 } do
					local id = if index == 1 then "A" else "B"
					local parent = Instance.new("Folder")
					parent.Name = "Repeated"
					parent.Parent = rootInstance
					expected[id] = parent
					virtualInstances[id] = {
						ClassName = "Folder",
						Name = "Repeated",
						Properties = {},
						Children = {},
						Metadata = { ignoreUnknownInstances = ignoreUnknown },
					}
					for leafIndex, definition in definitions[index] do
						local leafId = id .. "_" .. leafIndex
						local leaf = Instance.new(definition[2])
						leaf.Name = definition[1]
						leaf.Parent = parent
						expected[leafId] = leaf
						table.insert(virtualInstances[id].Children, leafId)
						virtualInstances[leafId] = {
							ClassName = definition[2],
							Name = definition[1],
							Properties = {},
							Children = {},
						}
					end
				end
				expectUnchanged(virtualInstances, rootInstance, expected, {})
			end
		end
	end)

	it("should ignore excluded children when distinguishing duplicate-named subtrees", function()
		for _, order in { { "A", "B" }, { "B", "A" } } do
			for _, temporary in
				{
					{ ClassName = "Folder", Name = "Temporary", Archivable = false },
					{ ClassName = "StringValue", Name = "Leaf", Archivable = false },
				}
			do
				expectUnchanged(duplicateTree(order, false, temporary, false))
			end
		end
	end)

	it("should ignore intentionally retained unknown children when distinguishing subtrees", function()
		for _, order in { { "A", "B" }, { "B", "A" } } do
			for _, temporary in
				{
					{ ClassName = "Folder", Name = "Temporary", Archivable = true },
					{ ClassName = "StringValue", Name = "Leaf", Archivable = true },
				}
			do
				expectUnchanged(duplicateTree(order, true, temporary, false))
			end
		end
	end)

	it("should retain order when extra same-name children leave no complete structural pairing", function()
		expectUnchanged(duplicateTree({ "A", "B" }, true, {
			ClassName = "Folder",
			Name = "Leaf",
			Archivable = true,
		}, false))
	end)

	it("should count explicitly described non-archivable children", function()
		expectUnchanged(duplicateTree({ "B", "A" }, false, nil, true))
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

	it("should honor cancellation at every step of duplicate-named candidate comparison", function()
		local virtualInstances, rootInstance = duplicateTree({ "B", "A" }, false, nil, false)
		local baseline = InstanceMap.new()
		local sizes = {}
		hydrate(baseline, virtualInstances, "ROOT", rootInstance, function()
			table.insert(sizes, baseline:size())
		end)
		baseline:stop()
		rootInstance:Destroy()

		for stopAt, expectedSize in sizes do
			local virtual, root = duplicateTree({ "B", "A" }, false, nil, false)
			local knownInstances = InstanceMap.new()
			local steps = 0
			local ok, failure = pcall(hydrate, knownInstances, virtual, "ROOT", root, function()
				steps += 1
				if steps == stopAt then
					error("cancel candidate comparison", 0)
				end
			end)
			expect(ok).to.equal(false)
			expect(failure).to.equal("cancel candidate comparison")
			expect(steps).to.equal(stopAt)
			expect(knownInstances:size()).to.equal(expectedSize)
			local stopped = table.clone(knownInstances.fromIds)
			task.wait()
			expect(knownInstances:size()).to.equal(expectedSize)
			for id, instance in stopped do
				expect(knownInstances.fromIds[id]).to.equal(instance)
			end
			knownInstances:stop()
			root:Destroy()
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
