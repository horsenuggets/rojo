return function()
	local Packages = script.Parent.Parent.Parent.Packages
	local Log = require(Packages.Log)

	local InstanceMap = require(script.Parent.Parent.InstanceMap)
	local PatchSet = require(script.Parent.Parent.PatchSet)

	local diff = require(script.Parent.diff)

	local function isEmpty(table)
		return next(table) == nil, "Table was not empty"
	end

	local function size(dict)
		local len = 0

		for _ in dict do
			len = len + 1
		end

		return len
	end

	it("should diff known parent changes once without adding or removing their identities", function()
		local applyPatch = require(script.Parent.applyPatch)
		for _, destinationKind in { "known", "added", "outside" } do
			local instanceMap = InstanceMap.new()
			local root = Instance.new("Folder")
			root.Name = "Root"
			local destination = Instance.new("Folder")
			destination.Name = "Destination"
			if destinationKind ~= "added" then
				destination.Parent = root
				instanceMap:insert("DESTINATION", destination)
			end
			local child = Instance.new("Folder")
			child.Name = "Before"
			child.Parent = if destinationKind == "outside" then destination else root
			instanceMap:insert("ROOT", root)
			instanceMap:insert("CHILD", child)
			local virtual = {
				ROOT = {
					Id = "ROOT",
					ClassName = "Folder",
					Name = "Root",
					Properties = {},
					Children = { "DESTINATION" },
				},
				DESTINATION = {
					Id = "DESTINATION",
					Parent = "ROOT",
					ClassName = "Folder",
					Name = "Destination",
					Properties = {},
					Children = { "CHILD" },
				},
				CHILD = {
					Id = "CHILD",
					Parent = "DESTINATION",
					ClassName = "Folder",
					Name = "After",
					Properties = {},
					Children = {},
				},
			}
			if destinationKind == "outside" then
				child.Parent = nil
			end
			local ok, patch = diff(instanceMap, virtual, "ROOT")
			assert(ok, "Expected a successful structural diff")
			expect(#patch.updated).to.equal(1)
			expect(patch.updated[1].id).to.equal("CHILD")
			expect(patch.updated[1].changedParent).to.equal("DESTINATION")
			expect(patch.updated[1].changedName).to.equal("After")
			expect(#patch.removed).to.equal(0)
			expect(patch.added.CHILD).to.equal(nil)
			expect(PatchSet.isEmpty(applyPatch(instanceMap, patch))).to.equal(true)
			expect(instanceMap.fromIds.CHILD).to.equal(child)
			local againOk, again = diff(instanceMap, virtual, "ROOT")
			assert(againOk, "Expected a successful diff after applying the move")
			expect(PatchSet.isEmpty(again)).to.equal(true)
			instanceMap:stop()
			root:Destroy()
			destination:Destroy()
		end
	end)

	it("should never reparent the synchronized root to the null-parent ID", function()
		for _, parentKind in { "nil", "unmanaged", "managed" } do
			local instanceMap = InstanceMap.new()
			local outer = Instance.new("Folder")
			local root = Instance.new("Folder")
			root.Name = "Root"
			if parentKind ~= "nil" then
				root.Parent = outer
			end
			if parentKind == "managed" then
				instanceMap:insert("OUTER", outer)
			end
			instanceMap:insert("ROOT", root)
			local virtual = {
				ROOT = {
					Id = "ROOT",
					Parent = "00000000000000000000000000000000",
					ClassName = "Folder",
					Name = "Root",
					Properties = {},
					Children = {},
				},
			}
			local ok, patch = diff(instanceMap, virtual, "ROOT")
			assert(ok, tostring(patch))
			expect(PatchSet.isEmpty(patch)).to.equal(true)
			virtual.ROOT.Name = "Renamed"
			local renamedOk, renamed = diff(instanceMap, virtual, "ROOT")
			assert(renamedOk, tostring(renamed))
			expect(#renamed.updated).to.equal(1)
			expect(renamed.updated[1].changedName).to.equal("Renamed")
			expect(renamed.updated[1].changedParent).to.equal(nil)
			instanceMap:stop()
			root:Destroy()
			outer:Destroy()
		end
	end)

	it("should generate an empty patch for empty instances", function()
		local knownInstances = InstanceMap.new()
		local virtualInstances = {
			ROOT = {
				ClassName = "Folder",
				Name = "Some Name",
				Properties = {},
				Children = {},
			},
		}

		local rootInstance = Instance.new("Folder")
		rootInstance.Name = "Some Name"
		knownInstances:insert("ROOT", rootInstance)

		local ok, patch = diff(knownInstances, virtualInstances, "ROOT")

		assert(ok, tostring(patch))

		assert(isEmpty(patch.removed))
		assert(isEmpty(patch.added))
		assert(isEmpty(patch.updated))
	end)

	it("should generate a patch with a changed name", function()
		local knownInstances = InstanceMap.new()
		local virtualInstances = {
			ROOT = {
				ClassName = "Folder",
				Name = "Some Name",
				Properties = {},
				Children = {},
			},
		}

		local rootInstance = Instance.new("Folder")
		knownInstances:insert("ROOT", rootInstance)

		local ok, patch = diff(knownInstances, virtualInstances, "ROOT")

		assert(ok, tostring(patch))

		assert(isEmpty(patch.removed))
		assert(isEmpty(patch.added))
		expect(#patch.updated).to.equal(1)

		local update = patch.updated[1]
		expect(update.id).to.equal("ROOT")
		expect(update.changedName).to.equal("Some Name")
		assert(isEmpty(update.changedProperties))
	end)

	it("should generate a patch with a changed property", function()
		local knownInstances = InstanceMap.new()
		local virtualInstances = {
			ROOT = {
				ClassName = "StringValue",
				Name = "Value",
				Properties = {
					Value = {
						String = "Hello, world!",
					},
				},
				Children = {},
			},
		}

		local rootInstance = Instance.new("StringValue")
		rootInstance.Value = "Initial Value"
		knownInstances:insert("ROOT", rootInstance)

		local ok, patch = diff(knownInstances, virtualInstances, "ROOT")

		assert(ok, tostring(patch))

		assert(isEmpty(patch.removed))
		assert(isEmpty(patch.added))
		expect(#patch.updated).to.equal(1)

		local update = patch.updated[1]
		expect(update.id).to.equal("ROOT")
		expect(update.changedName).to.equal(nil)
		expect(size(update.changedProperties)).to.equal(1)

		local patchProperty = update.changedProperties["Value"]
		expect(patchProperty).to.be.a("table")
		local ty, value = next(patchProperty)
		expect(ty).to.equal("String")
		expect(value).to.equal("Hello, world!")
	end)

	it("should generate an empty patch if no properties changed", function()
		local knownInstances = InstanceMap.new()
		local virtualInstances = {
			ROOT = {
				ClassName = "StringValue",
				Name = "Value",
				Properties = {
					Value = {
						String = "Hello, world!",
					},
				},
				Children = {},
			},
		}

		local rootInstance = Instance.new("StringValue")
		rootInstance.Value = "Hello, world!"
		knownInstances:insert("ROOT", rootInstance)

		local ok, patch = diff(knownInstances, virtualInstances, "ROOT")

		assert(ok, tostring(patch))
		assert(PatchSet.isEmpty(patch), "expected empty patch")
	end)

	it("should ignore unknown properties", function()
		local knownInstances = InstanceMap.new()
		local virtualInstances = {
			ROOT = {
				ClassName = "Folder",
				Name = "Folder",
				Properties = {
					FAKE_PROPERTY = {
						String = "Hello, world!",
					},
				},
				Children = {},
			},
		}

		local rootInstance = Instance.new("Folder")
		knownInstances:insert("ROOT", rootInstance)

		local ok, patch = diff(knownInstances, virtualInstances, "ROOT")

		assert(ok, tostring(patch))

		assert(isEmpty(patch.removed))
		assert(isEmpty(patch.added))
		assert(isEmpty(patch.updated))
	end)

	--[[
		Because rbx_dom_lua resolves non-canonical properties to their canonical
		variants, this test does not work as intended.

		Instead, heat_xml is diffed with Heat, the canonical property variant,
		and a patch trying to assign to heat_xml is generated. This is
		incorrect, but will require more invasive changes to fix later.
	]]
	itFIXME("should ignore unreadable properties", function()
		local knownInstances = InstanceMap.new()
		local virtualInstances = {
			ROOT = {
				ClassName = "Fire",
				Name = "Fire",
				Properties = {
					-- heat_xml is a serialization-only property that is not
					-- exposed to Lua.
					heat_xml = {
						Float32 = 5,
					},
				},
				Children = {},
			},
		}

		local rootInstance = Instance.new("Fire")
		knownInstances:insert("ROOT", rootInstance)

		local ok, patch = diff(knownInstances, virtualInstances, "ROOT")

		Log.warn("{:#?}", patch)

		assert(ok, tostring(patch))

		assert(isEmpty(patch.removed))
		assert(isEmpty(patch.added))
		assert(isEmpty(patch.updated))
	end)

	it("should generate a patch removing unknown children by default", function()
		local knownInstances = InstanceMap.new()
		local virtualInstances = {
			ROOT = {
				ClassName = "Folder",
				Name = "Folder",
				Properties = {},
				Children = {},
			},
		}

		local rootInstance = Instance.new("Folder")
		local unknownChild = Instance.new("Folder")
		unknownChild.Parent = rootInstance
		knownInstances:insert("ROOT", rootInstance)

		local ok, patch = diff(knownInstances, virtualInstances, "ROOT")

		assert(ok, tostring(patch))

		assert(isEmpty(patch.added))
		assert(isEmpty(patch.updated))
		expect(#patch.removed).to.equal(1)
		expect(patch.removed[1]).to.equal(unknownChild)
	end)

	it("should never remove the current camera while removing other unknown children", function()
		local Workspace = game:GetService("Workspace")
		local currentCamera = Workspace.CurrentCamera
		local createdCamera = nil
		if currentCamera == nil then
			createdCamera = Instance.new("Camera")
			createdCamera.Parent = Workspace
			Workspace.CurrentCamera = createdCamera
			currentCamera = createdCamera
		end
		local otherCamera = Instance.new("Camera")
		otherCamera.Name = "OtherCamera"
		otherCamera.Parent = Workspace
		local unknownPart = Instance.new("Part")
		unknownPart.Parent = Workspace

		local success, err = pcall(function()
			local knownInstances = InstanceMap.new()
			knownInstances:insert("ROOT", Workspace)
			local ok, patch = diff(knownInstances, {
				ROOT = {
					ClassName = "Workspace",
					Name = Workspace.Name,
					Properties = {},
					Children = {},
				},
			}, "ROOT")
			assert(ok, tostring(patch))

			local removed = {}
			for _, instance in patch.removed do
				removed[instance] = true
			end
			expect(removed[currentCamera]).to.equal(nil)
			expect(removed[otherCamera]).to.equal(true)
			expect(removed[unknownPart]).to.equal(true)
		end)

		otherCamera:Destroy()
		unknownPart:Destroy()
		if createdCamera then
			createdCamera:Destroy()
		end
		assert(success, err)
	end)

	it("should generate an empty patch if unknown children should be ignored", function()
		local knownInstances = InstanceMap.new()
		local virtualInstances = {
			ROOT = {
				ClassName = "Folder",
				Name = "Folder",
				Properties = {},
				Children = {},
				Metadata = {
					ignoreUnknownInstances = true,
				},
			},
		}

		local rootInstance = Instance.new("Folder")
		local unknownChild = Instance.new("Folder")
		unknownChild.Parent = rootInstance
		knownInstances:insert("ROOT", rootInstance)

		local ok, patch = diff(knownInstances, virtualInstances, "ROOT")

		assert(ok, tostring(patch))

		assert(isEmpty(patch.added))
		assert(isEmpty(patch.updated))
		assert(isEmpty(patch.removed))
	end)

	it("should generate a patch with an added child", function()
		local knownInstances = InstanceMap.new()
		local virtualInstances = {
			ROOT = {
				ClassName = "Folder",
				Name = "Folder",
				Properties = {},
				Children = { "CHILD" },
			},

			CHILD = {
				ClassName = "Folder",
				Name = "Child",
				Properties = {},
				Children = {},
			},
		}

		local rootInstance = Instance.new("Folder")
		knownInstances:insert("ROOT", rootInstance)

		local ok, patch = diff(knownInstances, virtualInstances, "ROOT")

		assert(ok, tostring(patch))

		assert(isEmpty(patch.updated))
		assert(isEmpty(patch.removed))
		expect(size(patch.added)).to.equal(1)
		expect(patch.added["CHILD"]).to.equal(virtualInstances["CHILD"])
	end)

	-- A known root with `width` known children, each renamed on the virtual
	-- side so the diff has an update to report for it.
	local function renamedTree(width)
		local knownInstances = InstanceMap.new()
		local virtualInstances = {
			ROOT = { ClassName = "Folder", Name = "Folder", Properties = {}, Children = {} },
		}
		local rootInstance = Instance.new("Folder")
		knownInstances:insert("ROOT", rootInstance)
		for index = 1, width do
			local id = "CHILD_" .. index
			virtualInstances[id] = { ClassName = "Folder", Name = "After" .. index, Properties = {}, Children = {} }
			table.insert(virtualInstances.ROOT.Children, id)

			local child = Instance.new("Folder")
			child.Name = "Before" .. index
			child.Parent = rootInstance
			knownInstances:insert(id, child)
		end
		return knownInstances, virtualInstances, rootInstance
	end

	-- The caller yields from `step`, so a walk that skipped it for part of the
	-- tree could still outlast Studio's script timeout.
	it("should call step once for every diffed instance", function()
		for _, width in { 0, 1, 9 } do
			local knownInstances, virtualInstances, rootInstance = renamedTree(width)
			local steps = 0
			local ok, patch = diff(knownInstances, virtualInstances, "ROOT", function()
				steps += 1
			end)
			assert(ok, tostring(patch))
			expect(steps).to.equal(1 + width)
			expect(#patch.updated).to.equal(width)
			knownInstances:stop()
			rootInstance:Destroy()
		end
	end)

	it("should compute the same patch when step yields", function()
		local knownInstances, virtualInstances, rootInstance = renamedTree(6)
		local ok, plain = diff(knownInstances, virtualInstances, "ROOT")
		assert(ok, tostring(plain))
		local yieldedOk, yielded = diff(knownInstances, virtualInstances, "ROOT", function()
			task.wait()
		end)
		assert(yieldedOk, tostring(yielded))

		expect(#yielded.updated).to.equal(#plain.updated)
		for index, update in plain.updated do
			expect(yielded.updated[index].id).to.equal(update.id)
			expect(yielded.updated[index].changedName).to.equal(update.changedName)
		end
		assert(isEmpty(yielded.added))
		assert(isEmpty(yielded.removed))
		knownInstances:stop()
		rootInstance:Destroy()
	end)
end
