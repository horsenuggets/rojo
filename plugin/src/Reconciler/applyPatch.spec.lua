return function()
	local applyPatch = require(script.Parent.applyPatch)

	local InstanceMap = require(script.Parent.Parent.InstanceMap)
	local PatchSet = require(script.Parent.Parent.PatchSet)

	local container = Instance.new("Folder")

	it("should close its profiling scope when applying a patch throws", function()
		local Reconciler = require(script.Parent)
		local Timer = require(script.Parent.Parent.Timer)
		local instanceMap = InstanceMap.new()
		local reconciler = Reconciler.new(instanceMap)
		local worker = coroutine.create(function()
			Timer.start("ApplyPatchTest.Outer")
			local ok = pcall(reconciler.applyPatch, reconciler, nil)
			local entries = Timer._entries[coroutine.running()]
			local count = #entries
			local label = entries[1].Label
			while Timer._entries[coroutine.running()] do
				Timer.stop()
			end
			return ok, count, label
		end)
		local resumed, applied, count, label = coroutine.resume(worker)
		instanceMap:stop()
		assert(resumed, applied)
		expect(applied).to.equal(false)
		expect(count).to.equal(1)
		expect(label).to.equal("ApplyPatchTest.Outer")
		expect(Timer._entries[worker]).to.equal(nil)
	end)

	-- The server announces a subtree Studio created. Studio already holds the
	-- root, so only a child that exists on disk alone may be created.
	it("should add only unknown children under an announced subtree Studio already has", function()
		local instanceMap = InstanceMap.new()
		local root = Instance.new("Folder")
		local created = Instance.new("Folder")
		created.Name = "Created"
		created.Parent = root
		instanceMap:insert("ROOT", root)
		instanceMap:insert("CREATED", created)
		local patch = PatchSet.newEmpty()
		patch.added = {
			CREATED = {
				Id = "CREATED",
				Parent = "ROOT",
				Name = "Created",
				ClassName = "Folder",
				Properties = {},
				Children = { "DISK_ONLY" },
			},
			DISK_ONLY = {
				Id = "DISK_ONLY",
				Parent = "CREATED",
				Name = "DiskOnly",
				ClassName = "Folder",
				Properties = {},
				Children = {},
			},
		}
		local unapplied = applyPatch(instanceMap, patch)
		assert(PatchSet.isEmpty(unapplied), "expected the whole patch to apply")
		expect(#root:GetChildren()).to.equal(1)
		expect(root.Created).to.equal(created)
		expect(#created:GetChildren()).to.equal(1)
		expect(instanceMap.fromIds.DISK_ONLY.Parent).to.equal(created)
		instanceMap:stop()
		root:Destroy()
	end)

	-- Metadata has nothing to apply in Studio. Leaving it unapplied would make
	-- the sync fallback replace the instance, alone or alongside a rename.
	it("should apply metadata changes without touching the instance", function()
		for _, renamed in { false, true } do
			local instanceMap = InstanceMap.new()
			local instance = Instance.new("Part")
			instance.Name = "Moved"
			instance.Parent = container
			instanceMap:insert("MOVED", instance)
			local patch = PatchSet.newEmpty()
			table.insert(patch.updated, {
				id = "MOVED",
				changedName = if renamed then "Renamed" else nil,
				changedMetadata = { ignoreUnknownInstances = false },
				changedProperties = {},
			})
			local unapplied = applyPatch(instanceMap, patch)
			assert(PatchSet.isEmpty(unapplied), "expected the whole patch to apply")
			expect(instanceMap.fromIds.MOVED).to.equal(instance)
			expect(instance.Name).to.equal(if renamed then "Renamed" else "Moved")
			instanceMap:stop()
			instance:Destroy()
		end
	end)

	it("should restore remove-and-add subtrees whether their old IDs are still mapped or absent", function()
		for _, mapped in { false, true } do
			local instanceMap = InstanceMap.new()
			local root = Instance.new("Folder")
			local original = Instance.new("Folder")
			original.Name = "Speculative"
			original.Parent = root
			local oldChild = Instance.new("Folder")
			oldChild.Parent = original
			instanceMap:insert("ROOT", root)
			if mapped then
				instanceMap:insert("RESTORED", original)
				instanceMap:insert("CHILD", oldChild)
			else
				original.Parent = nil
			end
			local patch = PatchSet.newEmpty()
			patch.removed = { "RESTORED" }
			patch.added = {
				RESTORED = {
					Id = "RESTORED",
					Parent = "ROOT",
					Name = "Authoritative",
					ClassName = "Folder",
					Properties = {},
					Children = { "CHILD" },
				},
				CHILD = {
					Id = "CHILD",
					Parent = "RESTORED",
					Name = "RestoredChild",
					ClassName = "Folder",
					Properties = {},
					Children = {},
				},
			}
			expect(PatchSet.isEmpty(applyPatch(instanceMap, patch))).to.equal(true)
			local restored = instanceMap.fromIds.RESTORED
			expect(restored ~= original).to.equal(true)
			expect(restored.Name).to.equal("Authoritative")
			expect(restored.Parent).to.equal(root)
			expect(instanceMap.fromIds.CHILD.Parent).to.equal(restored)
			expect(instanceMap.fromIds.CHILD.Name).to.equal("RestoredChild")
			expect(instanceMap.fromIds.CHILD ~= oldChild).to.equal(true)
			expect(#root:GetChildren()).to.equal(1)
			instanceMap:stop()
			root:Destroy()
			original:Destroy()
		end
	end)

	it("should restore children to a replaced authoritative parent after an unmanaged move", function()
		local createPatchSet = require(script.Parent.Parent.ChangeBatcher.createPatchSet)
		local instanceMap = InstanceMap.new()
		local root = Instance.new("Folder")
		local original = Instance.new("Folder")
		original.Name = "Parent"
		original.Parent = root
		local child = Instance.new("Folder")
		child.Parent = original
		local unmanaged = Instance.new("Folder")
		instanceMap:insert("ROOT", root)
		instanceMap:insert("PARENT", original)
		instanceMap:insert("CHILD", child)
		local patch = PatchSet.newEmpty()
		patch.updated = {
			{ id = "PARENT", changedClassName = "ModuleScript", changedProperties = {} },
		}
		assert(PatchSet.isEmpty(applyPatch(instanceMap, patch)))
		local replacement = instanceMap.fromIds.PARENT
		expect(replacement == original).to.equal(false)
		expect(child.Parent).to.equal(replacement)
		expect(instanceMap.knownParents[child]).to.equal(replacement)
		instanceMap:unpauseAllInstances()
		child.Parent = unmanaged
		local rejected = createPatchSet(instanceMap, { [child] = { Parent = true } })
		expect(PatchSet.isEmpty(rejected)).to.equal(true)
		expect(child.Parent).to.equal(replacement)
		expect(instanceMap.fromIds.CHILD).to.equal(child)
		instanceMap:stop()
		root:Destroy()
		original:Destroy()
		unmanaged:Destroy()
	end)

	it("should move the same instance with its children and references intact", function()
		local instanceMap = InstanceMap.new()
		local source = Instance.new("Folder")
		local destination = Instance.new("Folder")
		local part = Instance.new("Part")
		part.Parent = source
		local child = Instance.new("Folder")
		child.Parent = part
		local reference = Instance.new("ObjectValue")
		reference.Value = part
		instanceMap:insert("PART", part)
		instanceMap:insert("DESTINATION", destination)
		local patch = PatchSet.newEmpty()
		patch.updated = {
			{
				id = "PART",
				changedParent = "DESTINATION",
				changedName = "Moved",
				changedProperties = {
					Transparency = { Float32 = 0.5 },
				},
			},
		}
		local unapplied = applyPatch(instanceMap, patch)
		expect(PatchSet.isEmpty(unapplied)).to.equal(true)
		expect(instanceMap.fromIds.PART).to.equal(part)
		expect(part.Parent).to.equal(destination)
		expect(part.Name).to.equal("Moved")
		expect(part.Transparency).to.equal(0.5)
		expect(child.Parent).to.equal(part)
		expect(reference.Value).to.equal(part)
		expect(instanceMap.knownParents[part]).to.equal(destination)
		instanceMap:stop()
		source:Destroy()
		destination:Destroy()
		reference:Destroy()
	end)

	it("should preserve only unapplied structural changes for missing and cyclic destinations", function()
		for _, destinationId in { "MISSING", "CHILD", "PART" } do
			local instanceMap = InstanceMap.new()
			local source = Instance.new("Folder")
			local part = Instance.new("Part")
			part.Parent = source
			local child = Instance.new("Folder")
			child.Parent = part
			instanceMap:insert("PART", part)
			instanceMap:insert("CHILD", child)
			local patch = PatchSet.newEmpty()
			patch.updated = {
				{ id = "PART", changedParent = destinationId, changedName = "Kept", changedProperties = {} },
			}
			local unapplied = applyPatch(instanceMap, patch)
			expect(#unapplied.updated).to.equal(1)
			expect(unapplied.updated[1].changedParent).to.equal(destinationId)
			expect(unapplied.updated[1].changedName).to.equal(nil)
			expect(instanceMap.fromIds.PART).to.equal(part)
			expect(part.Parent).to.equal(source)
			expect(part.Name).to.equal("Kept")
			instanceMap:stop()
			source:Destroy()
		end
	end)

	it("should move known children into newly added parents without recreating them", function()
		local instanceMap = InstanceMap.new()
		local root = Instance.new("Folder")
		local child = Instance.new("Folder")
		child.Parent = root
		instanceMap:insert("ROOT", root)
		instanceMap:insert("CHILD", child)
		local patch = PatchSet.newEmpty()
		patch.added.NEW = {
			Id = "NEW",
			Parent = "ROOT",
			Name = "New",
			ClassName = "Folder",
			Properties = {},
			Children = { "CHILD" },
		}
		patch.updated = { { id = "CHILD", changedParent = "NEW", changedProperties = {} } }
		expect(PatchSet.isEmpty(applyPatch(instanceMap, patch))).to.equal(true)
		expect(instanceMap.fromIds.CHILD).to.equal(child)
		expect(child.Parent).to.equal(instanceMap.fromIds.NEW)
		instanceMap:stop()
		root:Destroy()
	end)

	it("should move retained descendants before removing their old ancestor", function()
		for _, destinationId in { "DESTINATION", "MISSING" } do
			local instanceMap = InstanceMap.new()
			local oldParent = Instance.new("Folder")
			local destination = Instance.new("Folder")
			local child = Instance.new("Folder")
			child.Parent = oldParent
			instanceMap:insert("OLD", oldParent)
			instanceMap:insert("DESTINATION", destination)
			instanceMap:insert("CHILD", child)
			local patch = PatchSet.newEmpty()
			patch.removed = { "OLD" }
			patch.updated = { { id = "CHILD", changedParent = destinationId, changedProperties = {} } }
			local unapplied = applyPatch(instanceMap, patch)
			expect(instanceMap.fromIds.CHILD).to.equal(child)
			if destinationId == "DESTINATION" then
				expect(child.Parent).to.equal(destination)
				expect(instanceMap.fromIds.OLD).to.equal(nil)
				expect(PatchSet.isEmpty(unapplied)).to.equal(true)
			else
				expect(child.Parent).to.equal(oldParent)
				expect(instanceMap.fromIds.OLD).to.equal(oldParent)
				expect(unapplied.removed[1]).to.equal("OLD")
			end
			instanceMap:stop()
			oldParent:Destroy()
			destination:Destroy()
		end
	end)

	it("should retry ancestry inversions after their destination moves out", function()
		local instanceMap = InstanceMap.new()
		local root = Instance.new("Folder")
		local parent = Instance.new("Folder")
		parent.Parent = root
		local child = Instance.new("Folder")
		child.Parent = parent
		instanceMap:insert("ROOT", root)
		instanceMap:insert("PARENT", parent)
		instanceMap:insert("CHILD", child)
		local patch = PatchSet.newEmpty()
		patch.updated = {
			{ id = "PARENT", changedParent = "CHILD", changedProperties = {} },
			{ id = "CHILD", changedParent = "ROOT", changedProperties = {} },
		}
		expect(PatchSet.isEmpty(applyPatch(instanceMap, patch))).to.equal(true)
		expect(parent.Parent).to.equal(child)
		expect(child.Parent).to.equal(root)
		instanceMap:stop()
		root:Destroy()
	end)

	it("should clear a reference when the server sends a null ref", function()
		local target = Instance.new("Folder")
		local pointer = Instance.new("ObjectValue")
		pointer.Value = target
		local instanceMap = InstanceMap.new()
		instanceMap:insert("POINTER", pointer)
		instanceMap:insert("TARGET", target)

		local unapplied = applyPatch(instanceMap, {
			removed = {},
			added = {},
			updated = {
				{
					id = "POINTER",
					changedProperties = {
						Value = { Ref = "00000000000000000000000000000000" },
					},
				},
			},
		})

		assert(PatchSet.isEmpty(unapplied), "expected the null ref to apply")
		expect(pointer.Value).to.equal(nil)
		pointer:Destroy()
		target:Destroy()
	end)

	local tempContainer = Instance.new("Folder")
	local function wasRemoved(instance)
		-- If an instance was destroyed, its parent property is locked.
		-- If an instance was removed, its parent property is nil.
		-- We need to ensure we only remove, so that ChangeHistoryService can still Undo.

		local isParentUnlocked = pcall(function()
			local oldParent = instance.Parent
			instance.Parent = tempContainer
			instance.Parent = oldParent
		end)

		return instance.Parent == nil and isParentUnlocked
	end

	beforeEach(function()
		container:ClearAllChildren()
	end)

	afterAll(function()
		container:Destroy()
		tempContainer:Destroy()
	end)

	it("should return an empty patch if given an empty patch", function()
		local patch = applyPatch(InstanceMap.new(), PatchSet.newEmpty())
		assert(PatchSet.isEmpty(patch), "expected remaining patch to be empty")
	end)

	it("should remove instances listed for remove", function()
		local root = Instance.new("Folder")
		root.Name = "ROOT"
		root.Parent = container

		local child = Instance.new("Folder")
		child.Name = "Child"
		child.Parent = root

		local instanceMap = InstanceMap.new()
		instanceMap:insert("ROOT", root)
		instanceMap:insert("CHILD", child)

		local patch = PatchSet.newEmpty()
		table.insert(patch.removed, child)

		local unapplied = applyPatch(instanceMap, patch)
		assert(PatchSet.isEmpty(unapplied), "expected remaining patch to be empty")

		assert(not wasRemoved(root), "expected root to be left alone")
		assert(wasRemoved(child), "expected child to be removed")

		instanceMap:stop()
	end)

	it("should remove IDs listed for remove", function()
		local root = Instance.new("Folder")
		root.Name = "ROOT"
		root.Parent = container

		local child = Instance.new("Folder")
		child.Name = "Child"
		child.Parent = root

		local instanceMap = InstanceMap.new()
		instanceMap:insert("ROOT", root)
		instanceMap:insert("CHILD", child)

		local patch = PatchSet.newEmpty()
		table.insert(patch.removed, "CHILD")

		local unapplied = applyPatch(instanceMap, patch)
		assert(PatchSet.isEmpty(unapplied), "expected remaining patch to be empty")
		expect(instanceMap:size()).to.equal(1)

		assert(not wasRemoved(root), "expected root to be left alone")
		assert(wasRemoved(child), "expected child to be removed")

		instanceMap:stop()
	end)

	it("should add instances to the DOM", function()
		-- Many of the details of this functionality are instead covered by
		-- tests on reify, not here.

		local root = Instance.new("Folder")
		root.Name = "ROOT"
		root.Parent = container

		local instanceMap = InstanceMap.new()
		instanceMap:insert("ROOT", root)

		local patch = PatchSet.newEmpty()
		patch.added["CHILD"] = {
			Id = "CHILD",
			ClassName = "Model",
			Name = "Child",
			Parent = "ROOT",
			Children = { "GRANDCHILD" },
			Properties = {},
		}

		patch.added["GRANDCHILD"] = {
			Id = "GRANDCHILD",
			ClassName = "Part",
			Name = "Grandchild",
			Parent = "CHILD",
			Children = {},
			Properties = {},
		}

		local unapplied = applyPatch(instanceMap, patch)
		assert(PatchSet.isEmpty(unapplied), "expected remaining patch to be empty")
		expect(instanceMap:size()).to.equal(3)

		local child = root:FindFirstChild("Child")
		expect(child).to.be.ok()
		expect(child.ClassName).to.equal("Model")
		expect(child).to.equal(instanceMap.fromIds["CHILD"])

		local grandchild = child:FindFirstChild("Grandchild")
		expect(grandchild).to.be.ok()
		expect(grandchild.ClassName).to.equal("Part")
		expect(grandchild).to.equal(instanceMap.fromIds["GRANDCHILD"])
	end)

	it("should return unapplied additions when instances cannot be created", function()
		local root = Instance.new("Folder")
		root.Name = "ROOT"
		root.Parent = container

		local instanceMap = InstanceMap.new()
		instanceMap:insert("ROOT", root)

		local patch = PatchSet.newEmpty()
		patch.added["OOPSIE"] = {
			Id = "OOPSIE",
			-- Hopefully Roblox never makes an instance with this ClassName.
			ClassName = "UH OH",
			Name = "FUBAR",
			Parent = "ROOT",
			Children = {},
			Properties = {},
		}

		local unapplied = applyPatch(instanceMap, patch)
		expect(unapplied.added["OOPSIE"]).to.equal(patch.added["OOPSIE"])
		expect(instanceMap:size()).to.equal(1)
		expect(#root:GetChildren()).to.equal(0)
	end)

	it("should apply property changes to instances", function()
		local value = Instance.new("StringValue")
		value.Value = "HELLO"

		local instanceMap = InstanceMap.new()
		instanceMap:insert("VALUE", value)

		local patch = PatchSet.newEmpty()
		table.insert(patch.updated, {
			id = "VALUE",
			changedProperties = {
				Value = {
					String = "WORLD",
				},
			},
		})

		local unapplied = applyPatch(instanceMap, patch)
		assert(PatchSet.isEmpty(unapplied), "expected remaining patch to be empty")
		expect(value.Value).to.equal("WORLD")
	end)

	it("should recreate instances when changedClassName is set, preserving children", function()
		local root = Instance.new("Folder")
		root.Name = "Initial Root Name"
		root.Parent = container

		local child = Instance.new("Folder")
		child.Name = "Child"
		child.Parent = root

		local instanceMap = InstanceMap.new()
		instanceMap:insert("ROOT", root)
		instanceMap:insert("CHILD", child)

		local patch = PatchSet.newEmpty()
		table.insert(patch.updated, {
			id = "ROOT",
			changedName = "Updated Root Name",
			changedClassName = "StringValue",
			changedProperties = {
				Value = {
					String = "I am Root",
				},
			},
		})

		local unapplied = applyPatch(instanceMap, patch)
		assert(PatchSet.isEmpty(unapplied), "expected remaining patch to be empty")

		local newRoot = instanceMap.fromIds["ROOT"]
		assert(newRoot ~= root, "expected instance to be recreated")
		expect(newRoot.ClassName).to.equal("StringValue")
		expect(newRoot.Name).to.equal("Updated Root Name")
		expect(newRoot.Value).to.equal("I am Root")

		local newChild = newRoot:FindFirstChild("Child")
		assert(newChild ~= nil, "expected child to be present")
		assert(newChild == child, "expected child to be preserved")
	end)
end
