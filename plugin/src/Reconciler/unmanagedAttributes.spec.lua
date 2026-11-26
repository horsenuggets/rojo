return function()
	local InstanceMap = require(script.Parent.Parent.InstanceMap)

	local applyPatch = require(script.Parent.applyPatch)
	local diff = require(script.Parent.diff)
	local unmanagedAttributes = require(script.Parent.unmanagedAttributes)

	local nan = 0 / 0

	it("should classify attributes the filesystem doesn't hold as unmanaged", function()
		local cases = {
			{ "Health", false },
			{ "Label", false },
			{ "RBX_ReimportId", true },
			{ "RBXRefinementScale", true },
			{ "Rojo_Id", true },
			{ "Rojo_Target_PrimaryPart", true },
			{ "Rojo_Other", false },
			{ "__RojoPlus_LiveEditUrl", true },
			{ "__RojoPlus_LiveEditSessionId", true },
			{ "__RojoPlus_LiveEditDirection", true },
			{ "__RojoPlus_LiveEditBootstrapped", true },
			{ "__RojoPlus_ConnectionUrl", true },
			{ "__Rojo_Other", false },
		}
		for _, case in cases do
			local name, expected = case[1], case[2]
			expect(unmanagedAttributes.isUnmanaged(name)).to.equal(expected)
		end
	end)

	-- Every combination of incoming and existing values having managed and
	-- unmanaged entries: managed ones always come from incoming, unmanaged ones
	-- always from the existing instance.
	it("should take managed attributes from incoming and unmanaged from existing", function()
		for bits = 0, 63 do
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
			if bit32.btest(bits, 16) then
				existing["__RojoPlus_LiveEditUrl"] = "localhost:34901"
			end
			if bit32.btest(bits, 32) then
				incoming["__RojoPlus_LiveEditUrl"] = "localhost:1"
			end

			local merged = unmanagedAttributes.merge(incoming, existing)
			expect(merged.Health ~= nil).to.equal(bit32.btest(bits, 1))
			expect(merged.RBX_Incoming).to.equal(nil)
			expect(merged.Stale).to.equal(nil)
			expect(merged.RBX_ReimportId ~= nil).to.equal(bit32.btest(bits, 8))
			-- The marker always comes from Studio, never from the filesystem.
			expect(merged["__RojoPlus_LiveEditUrl"]).to.equal(if bit32.btest(bits, 16) then "localhost:34901" else nil)
		end
	end)

	it("preserves directional session and bootstrap markers through attribute normalization", function()
		for _, name in
			{ "__RojoPlus_LiveEditSessionId", "__RojoPlus_LiveEditDirection", "__RojoPlus_LiveEditBootstrapped" }
		do
			for _, incoming in { {}, { [name] = "replace" } } do
				expect(unmanagedAttributes.merge(incoming, { [name] = "preserve" })[name]).to.equal("preserve")
				expect(unmanagedAttributes.merge(incoming, {})[name]).to.equal(nil)
			end
		end
	end)

	local function fixture()
		local part = Instance.new("Part")
		-- Scripts can't set reserved `RBX` attributes, and the attribute writer
		-- already skips them, so the fixture covers the other unmanaged kinds.
		part:SetAttribute("Health", 100)
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
		expect(part:GetAttribute("Rojo_Id")).to.equal("old")
		part:Destroy()
	end)

	-- A live edit place is built with the marker, but the tree that built it
	-- never holds one, so the first sync must not treat it as a deletion.
	it("should keep a baked live edit marker through the initial sync", function()
		local part = Instance.new("Part")
		part:SetAttribute("__RojoPlus_LiveEditUrl", "localhost:34901")
		local instanceMap = InstanceMap.new()
		instanceMap:insert("PART", part)

		local ok, patch = diff(instanceMap, {
			PART = {
				ClassName = "Part",
				Name = part.Name,
				Properties = {
					Attributes = { Attributes = {} },
				},
				Children = {},
			},
		}, "PART")
		assert(ok, tostring(patch))
		expect(#patch.updated).to.equal(0)
		applyPatch(instanceMap, patch)
		expect(part:GetAttribute("__RojoPlus_LiveEditUrl")).to.equal("localhost:34901")
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

	it("should apply and diff non-finite composite attributes from the filesystem", function()
		local part = Instance.new("Part")
		part:SetAttribute("Stale", Vector3.new(nan, 0, 0))
		local instanceMap = InstanceMap.new()
		instanceMap:insert("PART", part)
		local virtual = {
			PART = {
				ClassName = "Part",
				Name = part.Name,
				Properties = {
					Attributes = {
						Attributes = {
							Offset = { Vector3 = { nan, 1, -math.huge } },
							Range = { NumberRange = { 0, math.huge } },
						},
					},
				},
				Children = {},
			},
		}

		local ok, patch = diff(instanceMap, virtual, "PART")
		assert(ok, tostring(patch))
		expect(#patch.updated).to.equal(1)
		applyPatch(instanceMap, patch)
		local offset = part:GetAttribute("Offset")
		expect(offset.X ~= offset.X).to.equal(true)
		expect(offset.Z).to.equal(-math.huge)
		expect(part:GetAttribute("Range").Max).to.equal(math.huge)
		-- The filesystem holds NaN vectors now, so one it lacks is removed.
		expect(part:GetAttribute("Stale")).to.equal(nil)

		ok, patch = diff(instanceMap, virtual, "PART")
		assert(ok, tostring(patch))
		expect(#patch.updated).to.equal(0)
		part:Destroy()
	end)
end
