--[[

PatchSet.spec

Checks that structural moves count as updates even without ordinary properties.

--]]

return function()
	const PatchSet = require("./PatchSet")
	const summarizePatch = require("./summarizePatch")

	it("should count and summarize Parent alongside Name and properties", function()
		const instance = Instance.new("Folder")
		const instanceMap = { fromIds = { INSTANCE = instance } }
		const patch = PatchSet.newEmpty()
		patch.updated = { { id = "INSTANCE", changedParent = "PARENT", changedProperties = {} } }
		assert(PatchSet.validate(patch))
		expect(PatchSet.countChanges(patch)).to.equal(1)
		expect(PatchSet.countInstances(patch)).to.equal(1)
		expect(PatchSet.isEmpty(patch)).to.equal(false)
		expect(PatchSet.humanSummary(instanceMap, patch):find("Parent", 1, true) ~= nil).to.equal(true)
		expect(summarizePatch(patch, instanceMap, 10).updated[1].changedParent).to.equal("PARENT")
		patch.updated[1].changedName = "Moved"
		patch.updated[1].changedProperties.Archivable = { Bool = true }
		expect(PatchSet.countChanges(patch)).to.equal(3)
		instance:Destroy()
	end)
end
