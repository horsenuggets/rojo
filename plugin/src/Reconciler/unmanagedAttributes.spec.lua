return function()
	local InstanceMap = require(script.Parent.Parent.InstanceMap)

	local applyPatch = require(script.Parent.applyPatch)
	local diff = require(script.Parent.diff)
	local unmanagedAttributes = require(script.Parent.unmanagedAttributes)

	local nan = 0 / 0

	it("should classify attributes the filesystem can't hold as unmanaged", function()
		local cases = {
			{ "Health", 100, false },
			{ "Offset", Vector3.new(0, 1, 2), false },
			{ "Label", "text", false },
			{ "RBX_ReimportId", "id", true },
			{ "RBXRefinementScale", Vector3.one, true },
			{ "Rojo_Id", "id", true },
			{ "Rojo_Target_PrimaryPart", "id", true },
			{ "Rojo_Other", "x", false },
			{ "EmitDuration", nan, false },
			{ "Far", math.huge, false },
			{ "Near", -math.huge, false },
			{ "Broken", Vector3.new(nan, 0, 0), true },
			{ "Range", NumberRange.new(0, math.huge), true },
		}
		for _, case in cases do
			local name, value, expected = case[1], case[2], case[3]
			expect(unmanagedAttributes.isUnmanaged(name, value)).to.equal(expected)
		end
	end)

	-- Every combination of incoming and existing values having managed and
	-- unmanaged entries: managed ones always come from incoming, unmanaged ones
	-- always from the existing instance.
	it("should take managed attributes from incoming and unmanaged from existing", function()
		for bits = 0, 15 do
			local incoming = {}
			local existing = {}
			if bit32.btest(bits, 1) then
				incoming.Health = 1
			end
			if bit32.btest(bits, 2) then
				incoming.RBX_Incoming = "x"
			end
			if bit32.btest(bits, 4) then
				existing.Stale = 2
			end
			if bit32.btest(bits, 8) then
				existing.RBX_ReimportId = "keep"
			end

			local merged = unmanagedAttributes.merge(incoming, existing)
			expect(merged.Health ~= nil).to.equal(bit32.btest(bits, 1))
			expect(merged.RBX_Incoming).to.equal(nil)
			expect(merged.Stale).to.equal(nil)
			expect(merged.RBX_ReimportId ~= nil).to.equal(bit32.btest(bits, 8))
		end
	end)

	local function fixture()
		local part = Instance.new("Part")
		-- Scripts can't set reserved `RBX` attributes, and the attribute writer
		-- already skips them, so the fixture covers the other unmanaged kinds.
		part:SetAttribute("Health", 100)
		part:SetAttribute("Offset", Vector3.new(nan, 0, 0))
		part:SetAttribute("Rojo_Id", "old")
		local instanceMap = InstanceMap.new()
		instanceMap:insert("PART", part)
		return part, instanceMap
	end

	it("should not diff attributes the filesystem doesn't hold", function()
		local part, instanceMap = fixture()
		local ok, patch = diff(instanceMap, {
			PART = {
				ClassName = "Part",
				Name = part.Name,
				Properties = {
					Attributes = { Attributes = { Health = { Float64 = 100 } } },
				},
				Children = {},
			},
		}, "PART")
		assert(ok, tostring(patch))
		expect(#patch.updated).to.equal(0)
		part:Destroy()
	end)

	it("should keep unmanaged attributes when applying an attribute change", function()
		local part, instanceMap = fixture()
		applyPatch(instanceMap, {
			removed = {},
			added = {},
			updated = {
				{
					id = "PART",
					changedProperties = {
						Attributes = { Attributes = { Health = { Float64 = 5 } } },
					},
				},
			},
		})
		expect(part:GetAttribute("Health")).to.equal(5)
		local offset = part:GetAttribute("Offset")
		expect(offset.X ~= offset.X).to.equal(true)
		expect(part:GetAttribute("Rojo_Id")).to.equal("old")
		part:Destroy()
	end)

	it("should apply and diff infinite number attributes from the filesystem", function()
		local part = Instance.new("Part")
		local instanceMap = InstanceMap.new()
		instanceMap:insert("PART", part)
		local virtual = {
			PART = {
				ClassName = "Part",
				Name = part.Name,
				Properties = {
					Attributes = { Attributes = { EmitDuration = { Float64 = math.huge } } },
				},
				Children = {},
			},
		}

		local ok, patch = diff(instanceMap, virtual, "PART")
		assert(ok, tostring(patch))
		expect(#patch.updated).to.equal(1)
		applyPatch(instanceMap, patch)
		expect(part:GetAttribute("EmitDuration")).to.equal(math.huge)

		ok, patch = diff(instanceMap, virtual, "PART")
		assert(ok, tostring(patch))
		expect(#patch.updated).to.equal(0)
		part:Destroy()
	end)
end
