return function()
	local InstanceMap = require(script.Parent.Parent.InstanceMap)

	local diff = require(script.Parent.diff)
	local hydrate = require(script.Parent.hydrate)
	local services = require(script.Parent.services)

	it("should treat DataModel children and uncreatable classes as engine-owned", function()
		local Workspace = game:GetService("Workspace")
		local folder = Instance.new("Folder")
		folder.Parent = Workspace
		local camera = Instance.new("Camera")
		camera.Parent = Workspace
		local part = Instance.new("Part")
		part.Parent = Workspace
		local connection = part.Touched:Connect(function() end)

		local success, err = pcall(function()
			expect(services.isEngineOwned(Workspace)).to.equal(true)
			expect(services.isEngineOwned(game:GetService("ReplicatedStorage"))).to.equal(true)
			expect(services.isEngineOwned(folder)).to.equal(false)
			expect(services.isEngineOwned(camera)).to.equal(false)
			expect(services.isEngineOwned(part)).to.equal(false)

			local touchInterest = part:FindFirstChildOfClass("TouchTransmitter")
			if touchInterest then
				expect(services.isEngineOwned(touchInterest)).to.equal(true)
			end
		end)

		connection:Disconnect()
		part:Destroy()
		camera:Destroy()
		folder:Destroy()
		assert(success, err)
	end)

	it("should only resolve services under a DataModel", function()
		local folder = Instance.new("Folder")
		expect(services.getService(game, "Workspace")).to.equal(game:GetService("Workspace"))
		expect(services.getService(game, "Part")).to.equal(nil)
		expect(services.getService(game, "NotARealClass")).to.equal(nil)
		expect(services.getService(folder, "Workspace")).to.equal(nil)
		folder:Destroy()
	end)

	it("should hydrate a described service even when its name doesn't match", function()
		local teams = game:GetService("Teams")
		local originalName = teams.Name
		local success, err = pcall(function()
			teams.Name = "RenamedTeams"
			local instanceMap = InstanceMap.new()
			hydrate(instanceMap, {
				ROOT = { ClassName = "DataModel", Name = game.Name, Properties = {}, Children = { "TEAMS" } },
				TEAMS = { ClassName = "Teams", Name = "Teams", Properties = {}, Children = {} },
			}, "ROOT", game)
			expect(instanceMap.fromIds.TEAMS).to.equal(teams)
		end)
		teams.Name = originalName
		assert(success, err)
	end)

	it("should never add or remove children of the DataModel", function()
		local instanceMap = InstanceMap.new()
		local virtualInstances = {
			ROOT = { ClassName = "DataModel", Name = game.Name, Properties = {}, Children = { "HIDDEN" } },
			HIDDEN = {
				ClassName = "NotARealService",
				Name = "Hidden",
				Parent = "ROOT",
				Properties = {},
				Children = {},
			},
		}
		hydrate(instanceMap, virtualInstances, "ROOT", game)
		local ok, patch = diff(instanceMap, virtualInstances, "ROOT")
		assert(ok, tostring(patch))
		expect(patch.added.HIDDEN).to.equal(nil)
		for _, instance in patch.removed do
			expect(instance.Parent).never.to.equal(game)
		end
	end)
end
