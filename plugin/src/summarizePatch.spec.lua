return function()
	local InstanceMap = require(script.Parent.InstanceMap)
	local summarizePatch = require(script.Parent.summarizePatch)

	local function fixture()
		local root = Instance.new("Folder")
		root.Name = "Root"
		local existing = Instance.new("Part")
		existing.Name = "Existing"
		existing.Parent = root
		local unknown = Instance.new("Folder")
		unknown.Name = "Unknown"
		unknown.Parent = root

		local instanceMap = InstanceMap.new()
		instanceMap:insert("ROOT", root)
		instanceMap:insert("EXISTING", existing)

		local patch = {
			removed = { unknown },
			added = {
				NEW = { Parent = "ROOT", Name = "New", ClassName = "Folder", Properties = {}, Children = { "CHILD" } },
				CHILD = { Parent = "NEW", Name = "Child", ClassName = "Folder", Properties = {}, Children = {} },
			},
			updated = {
				{ id = "EXISTING", changedProperties = { Transparency = {}, Anchored = {} } },
			},
		}
		return root, patch, instanceMap
	end

	it("should describe removals, added roots, and updated properties", function()
		local root, patch, instanceMap = fixture()
		local summary = summarizePatch(patch, instanceMap, 10)

		expect(summary.counts.removed).to.equal(1)
		expect(summary.counts.added).to.equal(2)
		expect(summary.counts.addedRoots).to.equal(1)
		expect(summary.counts.updated).to.equal(1)
		expect(summary.removed[1]).to.equal("Root.Unknown")
		expect(#summary.added).to.equal(1)
		expect(summary.added[1]).to.equal("Root.New")
		expect(summary.updated[1].path).to.equal("Root.Existing")
		expect(table.concat(summary.updated[1].properties, ",")).to.equal("Anchored,Transparency")
		root:Destroy()
	end)

	it("should cap listed entries while keeping complete counts", function()
		local root, patch, instanceMap = fixture()
		local summary = summarizePatch(patch, instanceMap, 0)

		expect(#summary.removed).to.equal(0)
		expect(#summary.added).to.equal(0)
		expect(#summary.updated).to.equal(0)
		expect(summary.counts.removed).to.equal(1)
		expect(summary.counts.addedRoots).to.equal(1)
		expect(summary.counts.updated).to.equal(1)
		root:Destroy()
	end)

	it("should return the same summary for the same patch", function()
		local root, patch, instanceMap = fixture()
		local first = summarizePatch(patch, instanceMap, 10)
		local second = summarizePatch(patch, instanceMap, 10)
		expect(first.removed[1]).to.equal(second.removed[1])
		expect(first.added[1]).to.equal(second.added[1])
		expect(first.updated[1].path).to.equal(second.updated[1].path)
		root:Destroy()
	end)
end
