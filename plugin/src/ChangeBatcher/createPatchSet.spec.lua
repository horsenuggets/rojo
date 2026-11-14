return function()
	local PatchSet = require(script.Parent.Parent.PatchSet)
	local InstanceMap = require(script.Parent.Parent.InstanceMap)

	local createPatchSet = require(script.Parent.createPatchSet)

	it("should return a patch", function()
		local patch = createPatchSet(InstanceMap.new(), {})

		assert(PatchSet.validate(patch))
	end)

	it("should contain updates for every instance with property changes", function()
		local instanceMap = InstanceMap.new()

		local part1 = Instance.new("Part")
		instanceMap:insert("PART_1", part1)

		local part2 = Instance.new("Part")
		instanceMap:insert("PART_2", part2)

		local changes = {
			[part1] = {
				Position = true,
				Size = true,
				Color = true,
			},
			[part2] = {
				CFrame = true,
				Velocity = true,
				Transparency = true,
			},
		}

		local patch = createPatchSet(instanceMap, changes)

		expect(#patch.updated).to.equal(2)
	end)

	it("should not contain any updates for removed instances", function()
		local instanceMap = InstanceMap.new()

		local part1 = Instance.new("Part")
		instanceMap:insert("PART_1", part1)

		local changes = {
			[part1] = {
				Parent = true,
				Position = true,
				Size = true,
			},
		}

		local patch = createPatchSet(instanceMap, changes)

		expect(#patch.removed).to.equal(1)
		expect(#patch.updated).to.equal(0)
	end)

	it("should omit unsupported properties without blocking other updates", function()
		local instanceMap = InstanceMap.new()
		local objectValue = Instance.new("ObjectValue")
		objectValue.Value = Instance.new("Part")
		instanceMap:insert("REFERENCE", objectValue)
		local part = Instance.new("Part")
		instanceMap:insert("PART", part)
		local changes = {
			[objectValue] = { Value = true },
			[part] = { Transparency = true },
		}
		local patch = createPatchSet(instanceMap, changes)
		expect(#patch.updated).to.equal(1)
		expect(patch.updated[1].id).to.equal("PART")
		expect(next(changes)).to.equal(nil)
	end)

	it("should omit computed properties while preserving editable changes", function()
		local instanceMap = InstanceMap.new()
		local part = Instance.new("Part")
		instanceMap:insert("PART", part)
		local patch = createPatchSet(instanceMap, {
			[part] = { AssemblyCenterOfMass = true, Transparency = true },
		})
		expect(#patch.updated).to.equal(1)
		expect(patch.updated[1].changedProperties.AssemblyCenterOfMass).to.equal(nil)
		expect(patch.updated[1].changedProperties.Transparency.Float32).to.equal(part.Transparency)
		local computedOnly = createPatchSet(instanceMap, {
			[part] = { AssemblyCenterOfMass = true },
		})
		expect(PatchSet.isEmpty(computedOnly)).to.equal(true)
		part:Destroy()
	end)

	it("should omit computed-only updates across instance classes", function()
		for className, propertyName in
			{
				Part = "AssemblyMass",
				Frame = "AbsoluteSize",
				TextLabel = "TextBounds",
				Humanoid = "MoveDirection",
			}
		do
			local instanceMap = InstanceMap.new()
			local instance = Instance.new(className)
			instanceMap:insert("INSTANCE", instance)
			local changes = { [instance] = { [propertyName] = true } }
			local patch = createPatchSet(instanceMap, changes)
			expect(PatchSet.isEmpty(patch)).to.equal(true)
			expect(next(changes)).to.equal(nil)
			instance:Destroy()
		end
	end)

	it("should preserve resize and CFrame edits without serializing their side effects", function()
		local instanceMap = InstanceMap.new()
		local part = Instance.new("Part")
		part.Size = Vector3.new(4, 5, 6)
		part.CFrame = CFrame.new(1, 2, 3) * CFrame.Angles(0.1, 0.2, 0.3)
		instanceMap:insert("PART", part)
		local patch = createPatchSet(instanceMap, {
			[part] = {
				CFrame = true,
				LocalTransparencyModifier = true,
				Orientation = true,
				Position = true,
				Rotation = true,
				Size = true,
			},
		})
		expect(#patch.updated).to.equal(1)
		local properties = patch.updated[1].changedProperties
		expect(properties.CFrame ~= nil).to.equal(true)
		expect(properties.Size ~= nil).to.equal(true)
		for _, propertyName in { "LocalTransparencyModifier", "Orientation", "Position", "Rotation" } do
			expect(properties[propertyName]).to.equal(nil)
		end
		local excludedOnly = createPatchSet(instanceMap, {
			[part] = { LocalTransparencyModifier = true, Orientation = true, Position = true, Rotation = true },
		})
		expect(PatchSet.isEmpty(excludedOnly)).to.equal(true)
		part:Destroy()
	end)

	it("should encode managed references using existing Rojo IDs", function()
		local instanceMap = InstanceMap.new()
		local target = Instance.new("Part")
		instanceMap:insert("TARGET", target)
		local objectValue = Instance.new("ObjectValue")
		objectValue.Value = target
		instanceMap:insert("REFERENCE", objectValue)
		local patch = createPatchSet(instanceMap, {
			[objectValue] = { Value = true },
		})
		expect(#patch.updated).to.equal(1)
		expect(patch.updated[1].changedProperties.Value.Ref).to.equal("TARGET")
	end)

	it("should silently discard unmanaged changes without blocking managed updates", function()
		local Log = require(script.Parent.Parent.Parent.Packages.Log)
		local originalWarn = Log.warn
		local camera = Instance.new("Camera")
		local part = Instance.new("Part")
		local success, err = pcall(function()
			Log.warn = function()
				error("Unmanaged changes must not warn")
			end
			local instanceMap = InstanceMap.new()
			instanceMap:insert("PART", part)
			local changes = {
				[camera] = { CFrame = true },
				[part] = { Transparency = true },
			}
			local patch = createPatchSet(instanceMap, changes)
			expect(#patch.updated).to.equal(1)
			expect(patch.updated[1].id).to.equal("PART")
			expect(#patch.removed).to.equal(0)
			expect(next(changes)).to.equal(nil)
			expect(PatchSet.isEmpty(createPatchSet(instanceMap, changes))).to.equal(true)
		end)
		Log.warn = originalWarn
		camera:Destroy()
		part:Destroy()
		assert(success, err)
	end)

	it("should never write changes to the DataModel", function()
		local instanceMap = InstanceMap.new()
		instanceMap:insert("ROOT", game)
		local part = Instance.new("Part")
		instanceMap:insert("PART", part)
		local changes = {
			[game] = { Name = true },
			[part] = { Transparency = true },
		}
		local patch = createPatchSet(instanceMap, changes)
		expect(#patch.updated).to.equal(1)
		expect(patch.updated[1].id).to.equal("PART")
		expect(next(changes)).to.equal(nil)
		part:Destroy()
	end)

	it("should remove instances from the property change table", function()
		local instanceMap = InstanceMap.new()

		local part1 = Instance.new("Part")
		instanceMap:insert("PART_1", part1)

		local changes = {
			[part1] = {},
		}

		createPatchSet(instanceMap, changes)

		expect(next(changes)).to.equal(nil)
	end)
end
