return function()
	local InstanceMap = require(script.Parent.Parent.InstanceMap)

	local applyPatch = require(script.Parent.applyPatch)
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

	it("should keep protected instances when a patch removes or replaces them", function()
		local Workspace = game:GetService("Workspace")
		local camera = Workspace.CurrentCamera
		local createdCamera = nil
		if camera == nil then
			createdCamera = Instance.new("Camera")
			createdCamera.Parent = Workspace
			Workspace.CurrentCamera = createdCamera
			camera = createdCamera
		end
		local cameraParent = camera.Parent
		local replicatedStorage = game:GetService("ReplicatedStorage")
		local folder = Instance.new("Folder")
		folder.Parent = Workspace

		local success, err = pcall(function()
			local instanceMap = InstanceMap.new()
			instanceMap:insert("CAMERA", camera)
			instanceMap:insert("STORAGE", replicatedStorage)
			instanceMap:insert("FOLDER", folder)

			local unapplied = applyPatch(instanceMap, {
				removed = { "CAMERA", replicatedStorage, "FOLDER" },
				added = {},
				updated = {
					{ id = "STORAGE", changedClassName = "Folder", changedProperties = {} },
				},
			})

			expect(camera.Parent).to.equal(cameraParent)
			expect(replicatedStorage.Parent).to.equal(game)
			expect(folder.Parent).to.equal(nil)
			expect(instanceMap.fromIds.CAMERA).to.equal(nil)
			expect(instanceMap.fromIds.FOLDER).to.equal(nil)
			expect(#unapplied.removed).to.equal(0)
			-- The class change is reported as not applied instead of replacing
			-- the service.
			expect(#unapplied.updated).to.equal(1)
			expect(unapplied.updated[1].id).to.equal("STORAGE")
		end)

		folder:Destroy()
		if createdCamera then
			createdCamera:Destroy()
		end
		assert(success, err)
	end)

	it("should keep protected updates out of sync fallback replacement", function()
		local Workspace = game:GetService("Workspace")
		local camera = Workspace.CurrentCamera
		local createdCamera = nil
		if camera == nil then
			createdCamera = Instance.new("Camera")
			createdCamera.Parent = Workspace
			Workspace.CurrentCamera = createdCamera
			camera = createdCamera
		end
		local folder = Instance.new("Folder")
		folder.Parent = Workspace

		local success, err = pcall(function()
			local instanceMap = InstanceMap.new()
			instanceMap:insert("CAMERA", camera)
			instanceMap:insert("STORAGE", game:GetService("ReplicatedStorage"))
			instanceMap:insert("FOLDER", folder)

			local replaceable, protected = services.splitReplaceableUpdates({
				{ id = "CAMERA", changedClassName = "Folder", changedProperties = {} },
				{ id = "FOLDER", changedClassName = "Model", changedProperties = {} },
				{ id = "STORAGE", changedClassName = "Folder", changedProperties = {} },
				{ id = "MISSING", changedProperties = {} },
			}, instanceMap)

			expect(#replaceable).to.equal(2)
			expect(replaceable[1].id).to.equal("FOLDER")
			expect(replaceable[2].id).to.equal("MISSING")
			expect(#protected).to.equal(2)
			expect(protected[1].id).to.equal("CAMERA")
			expect(protected[2].id).to.equal("STORAGE")
		end)

		folder:Destroy()
		if createdCamera then
			createdCamera:Destroy()
		end
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
