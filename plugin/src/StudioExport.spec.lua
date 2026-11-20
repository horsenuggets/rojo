return function()
	const StudioExport = require(script.Parent.StudioExport)
	const Packages = script.Parent.Parent.Packages
	const Http = require(Packages.Http)
	const Promise = require(Packages.Promise)
	const RbxDom = require(Packages.RbxDom)
	const Types = require(script.Parent.Types)

	const function request(scope)
		return {
			jobId = "test-job",
			scope = scope or "model",
			paths = { "Root" },
			selection = false,
			allowIncomplete = false,
			partSize = 1048576,
		}
	end

	const function fixture()
		const root = Instance.new("Folder")
		const model = Instance.new("Folder")
		model.Name = "Root"
		model.Parent = root
		const child = Instance.new("Folder")
		child.Name = "Child"
		child.Parent = model
		return root, model, child
	end

	const function boundaries(root)
		return {
			root = root,
			isRunning = function()
				return false
			end,
			selection = function()
				return {}
			end,
			serialize = function()
				return "native bytes"
			end,
			serviceProperties = function(instance)
				return { Name = { String = instance.Name } }
			end,
			post = function()
				return Promise.resolve({ code = 200 })
			end,
		}
	end

	it("resolves exact direct paths with an optional game prefix", function()
		const root, _, child = fixture()
		expect(StudioExport.resolvePath(root, "Root/Child")).to.equal(child)
		expect(StudioExport.resolvePath(root, "game/Root/Child")).to.equal(child)
		for _, path in { "", "game", "Root/", "/Root", "root", "Root/Missing" } do
			expect(pcall(StudioExport.resolvePath, root, path)).to.equal(false)
		end
		const duplicate = Instance.new("Folder")
		duplicate.Name = "Root"
		duplicate.Parent = root
		expect(pcall(StudioExport.resolvePath, root, "Root/Child")).to.equal(false)
		root:Destroy()
	end)

	it("native serialization keeps identical root labels in input order with cross-root references", function()
		const serializer = game:GetService("SerializationService")
		const parents = {}
		const roots = {}
		for index = 1, 3 do
			const parent = Instance.new("Folder")
			const root = Instance.new("Model")
			root.Name = "Same"
			root:SetAttribute("CaptureOrder", index)
			root.Parent = parent
			parents[index] = parent
			roots[index] = root
		end
		for index, root in roots do
			const link = Instance.new("ObjectValue")
			link.Name = "Link"
			link.Value = roots[index % #roots + 1]
			link.Parent = root
		end
		for _, order in { { 1, 2, 3 }, { 1, 3, 2 }, { 2, 1, 3 }, { 2, 3, 1 }, { 3, 1, 2 }, { 3, 2, 1 } } do
			const selected = {}
			for _, index in order do
				table.insert(selected, roots[index])
			end
			const loaded = serializer:DeserializeInstancesAsync(serializer:SerializeInstancesAsync(selected))
			expect(#loaded).to.equal(#order)
			for index, root in loaded do
				expect(root.Name).to.equal("Same")
				expect(root:GetAttribute("CaptureOrder")).to.equal(order[index])
				expect(root.Link.Value:GetAttribute("CaptureOrder")).to.equal(order[index] % #roots + 1)
			end
			for _, root in loaded do
				root:Destroy()
			end
		end
		for _, parent in parents do
			parent:Destroy()
		end
	end)

	it("normalizes duplicates and nested roots in stable input order", function()
		const root, model, child = fixture()
		const sibling = Instance.new("Folder")
		sibling.Parent = root
		const normalized = StudioExport.normalizeRoots({ child, sibling, model, model, child })
		expect(#normalized).to.equal(2)
		expect(normalized[1]).to.equal(sibling)
		expect(normalized[2]).to.equal(model)
		root:Destroy()
	end)

	it("captures all chosen roots in one call and reports excluded descendant subtrees", function()
		const root, model, child = fixture()
		child.Archivable = false
		const grandchild = Instance.new("Folder")
		grandchild.Parent = child
		const sibling = Instance.new("Folder")
		sibling.Name = "Other"
		sibling.Parent = root
		const boundary = boundaries(root)
		local calls = 0
		boundary.serialize = function(instances)
			calls += 1
			expect(#instances).to.equal(2)
			expect(instances[1]).to.equal(model)
			expect(instances[2]).to.equal(sibling)
			return "captured"
		end
		const body = request()
		body.paths = { "Root", "Other" }
		const contents, manifest = StudioExport.capture(body, boundary)
		expect(contents).to.equal("captured")
		expect(calls).to.equal(1)
		expect(manifest.roots[1].descendantCount).to.equal(0)
		expect(#manifest.warnings).to.equal(1)
		expect(child.Archivable).to.equal(false)
		expect(grandchild.Parent).to.equal(child)
		root:Destroy()
	end)

	it("combines paths and selection without serializing nested roots twice", function()
		const root, model, child = fixture()
		const boundary = boundaries(root)
		boundary.selection = function()
			return { child, model }
		end
		const body = request()
		body.selection = true
		const _, manifest = StudioExport.capture(body, boundary)
		expect(#manifest.roots).to.equal(1)
		expect(manifest.roots[1].descendantCount).to.equal(1)
		expect(#manifest.warnings).to.equal(0)
		root:Destroy()
	end)

	it("only warns for references outside the complete archivable selection", function()
		for _, selection in { false, true } do
			for _, external in { false, true } do
				for _, excluded in { false, true } do
					const root, model, child = fixture()
					child.Archivable = not excluded
					const reference = Instance.new("ObjectValue")
					reference.Parent = model
					reference.Value = if external then root else child
					const boundary = boundaries(root)
					boundary.selection = function()
						return { model }
					end
					const body = request()
					body.selection = selection
					body.paths = if selection then {} else { "Root" }
					const _, manifest = StudioExport.capture(body, boundary)
					local references = 0
					for _, warning in manifest.warnings do
						if string.find(warning, "External reference", 1, true) then
							references += 1
						end
					end
					expect(references).to.equal(if external or excluded then 1 else 0)
					expect(#manifest.warnings).to.equal((if excluded then 1 else 0) + references)
					root:Destroy()
				end
			end
		end
	end)

	it("preserves references between separately selected roots without warnings", function()
		const root, model = fixture()
		const other = Instance.new("Folder")
		other.Name = "Other"
		other.Parent = root
		const reference = Instance.new("ObjectValue")
		reference.Parent = model
		reference.Value = other
		const body = request()
		body.paths = { "Root", "Other" }
		const _, manifest = StudioExport.capture(body, boundaries(root))
		expect(#manifest.warnings).to.equal(0)
		root:Destroy()
	end)

	it("refuses missing selection, service roots, detached roots, and play mode", function()
		const root, model = fixture()
		const boundary = boundaries(root)
		const body = request()
		body.paths = {}
		expect(pcall(StudioExport.capture, body, boundary)).to.equal(false)
		body.selection = true
		for _, selected in { root, game:GetService("Workspace"), Instance.new("Folder") } do
			boundary.selection = function()
				return { selected }
			end
			expect(pcall(StudioExport.capture, body, boundary)).to.equal(false)
		end
		body.paths = { "Root" }
		body.selection = false
		model.Archivable = false
		expect(pcall(StudioExport.capture, body, boundary)).to.equal(false)
		model.Archivable = true
		boundary.isRunning = function()
			return true
		end
		expect(pcall(StudioExport.capture, body, boundary)).to.equal(false)
		root:Destroy()
	end)

	it("requires incomplete consent before whole-place capture", function()
		const root = fixture()
		const boundary = boundaries(root)
		boundary.serialize = function()
			error("Serialization must not be reached.")
		end
		const ok, err = pcall(StudioExport.capture, request("place"), boundary)
		expect(ok).to.equal(false)
		expect(string.find(tostring(err), "allowIncomplete", 1, true) ~= nil).to.equal(true)
		root:Destroy()
	end)

	it("excludes runtime players while preserving serializable children of Players", function()
		for playerCount = 0, 2 do
			const savedChild = Instance.new("Folder")
			savedChild.Name = "Player"
			const children = { savedChild }
			for index = 1, playerCount do
				table.insert(children, {
					Archivable = true,
					ClassName = "Player",
					Name = `Transient{index}`,
					GetFullName = function(self)
						return `Players.{self.Name}`
					end,
					GetChildren = function()
						error("Runtime player subtrees must not be captured.")
					end,
				})
			end
			const service = {
				ClassName = "Players",
				Name = "Players",
				GetChildren = function()
					return children
				end,
			}
			const boundary = boundaries({
				GetChildren = function()
					return { service }
				end,
			})
			boundary.serialize = function(instances)
				expect(#instances).to.equal(1)
				expect(instances[1]).to.equal(savedChild)
				return "persistent player-service content"
			end
			const body = request("place")
			body.paths = {}
			body.allowIncomplete = true
			const contents, manifest = StudioExport.capture(body, boundary)
			expect(contents).to.equal("persistent player-service content")
			expect(#manifest.roots).to.equal(1)
			expect(manifest.roots[1].name).to.equal("Player")
			expect(manifest.roots[1].className).to.equal("Folder")
			expect(manifest.roots[1].service).to.equal("Players")
			expect(manifest.roots[1].descendantCount).to.equal(0)
			expect(#manifest.services).to.equal(1)
			expect(manifest.services[1].className).to.equal("Players")
			expect(#manifest.warnings).to.equal(1 + playerCount)
			savedChild:Destroy()
		end
	end)

	it("captures native persisted services and their children when they have the Service tag", function()
		for _, className in
			{
				"AssetService",
				"AvatarSettings",
				"HttpService",
				"InsertService",
				"PhysicsService",
				"SerializationService",
				"TestService",
				"TweenService",
			}
		do
			const child = Instance.new("Folder")
			child.Name = "Persisted"
			const service = {
				Archivable = true,
				ClassName = className,
				Name = className,
				GetChildren = function()
					return { child }
				end,
			}
			const boundary = boundaries({
				GetChildren = function()
					return { service }
				end,
			})
			boundary.serialize = function(instances)
				expect(#instances).to.equal(1)
				expect(instances[1]).to.equal(child)
				return "mapped service content"
			end
			const body = request("place")
			body.paths = {}
			body.allowIncomplete = true
			const contents, manifest = StudioExport.capture(body, boundary)
			expect(contents).to.equal("mapped service content")
			expect(#manifest.services).to.equal(1)
			expect(manifest.services[1].className).to.equal(className)
			expect(#manifest.roots).to.equal(1)
			expect(manifest.roots[1].name).to.equal("Persisted")
			expect(manifest.roots[1].service).to.equal(className)
			expect(#manifest.warnings).to.equal(1)
			child:Destroy()
		end
	end)

	it("serializes Terrain and its children alongside other live service children", function()
		const child = Instance.new("Folder")
		child.Name = "Live"
		const clouds = Instance.new("Clouds")
		const terrain = {
			Archivable = true,
			Name = "Terrain",
			ClassName = "Terrain",
			GetChildren = function()
				return { clouds }
			end,
		}
		-- A fake service boundary avoids mutating any real service in Studio.
		const service = {
			Name = "Workspace",
			ClassName = "Workspace",
			GetChildren = function()
				return { child, terrain }
			end,
		}
		const boundary = boundaries({
			GetChildren = function()
				return { service }
			end,
		})
		boundary.serialize = function(instances)
			expect(#instances).to.equal(2)
			expect(instances[1]).to.equal(child)
			expect(instances[2]).to.equal(terrain)
			return "terrain and model bytes"
		end
		const body = request("place")
		body.allowIncomplete = true
		const contents, manifest = StudioExport.capture(body, boundary)
		expect(contents).to.equal("terrain and model bytes")
		expect(manifest.roots[1].service).to.equal("Workspace")
		expect(manifest.services[1].properties.Name.String).to.equal("Workspace")
		expect(#manifest.roots).to.equal(2)
		expect(manifest.roots[2].name).to.equal("Terrain")
		expect(manifest.roots[2].className).to.equal("Terrain")
		expect(manifest.roots[2].service).to.equal("Workspace")
		expect(manifest.roots[2].descendantCount).to.equal(1)
		expect(#manifest.warnings).to.equal(1)
		clouds:Destroy()
		child:Destroy()
	end)

	it("resolves service references by captured identity after collecting every root", function()
		const camera = Instance.new("Camera")
		const duplicate = Instance.new("Camera")
		camera.Name = "Same"
		duplicate.Name = "Same"
		const external = Instance.new("Camera")
		const workspaceService = {
			Name = "Workspace",
			ClassName = "Workspace",
			GetChildren = function()
				return { duplicate, camera }
			end,
		}
		const lighting = {
			Name = "Lighting",
			ClassName = "Lighting",
			GetChildren = function()
				return {}
			end,
		}
		const boundary = boundaries({
			GetChildren = function()
				return { workspaceService, lighting }
			end,
		})
		boundary.serviceProperties = function(service)
			return { Name = { String = service.Name } },
				if service == lighting
					then {
						{ property = "LaterRoot", target = camera },
						{ property = "LaterService", target = workspaceService },
						{ property = "External", target = external },
					}
					else { { property = "CurrentCamera", target = camera } }
		end
		const body = request("place")
		body.paths = {}
		body.allowIncomplete = true
		const _, manifest = StudioExport.capture(body, boundary)
		const before = manifest.services[1].references
		expect(before[1].property).to.equal("LaterRoot")
		expect(before[1].target.kind).to.equal("root")
		expect(before[1].target.index).to.equal(1)
		expect(before[2].property).to.equal("LaterService")
		expect(before[2].target.kind).to.equal("service")
		expect(before[2].target.index).to.equal(1)
		expect(#before).to.equal(2)
		const captured = manifest.services[2].references
		expect(captured[1].property).to.equal("CurrentCamera")
		expect(captured[1].target.kind).to.equal("root")
		expect(captured[1].target.index).to.equal(1)
		expect(#manifest.warnings).to.equal(2)
		expect(manifest.warnings[2]:find("Lighting.External", 1, true) ~= nil).to.equal(true)
		expect(camera.Parent).to.equal(nil)
		camera:Destroy()
		duplicate:Destroy()
		external:Destroy()
	end)

	it("retains readable non-null service references for deferred capture mapping", function()
		const camera = Instance.new("Camera")
		const properties, references = StudioExport.serviceProperties({
			Name = "Workspace",
			ClassName = "Workspace",
			CurrentCamera = camera,
			GetAttributes = function()
				return {}
			end,
		}, {})
		expect(properties.CurrentCamera).to.equal(nil)
		local found = false
		for _, reference in references do
			if reference.property == "CurrentCamera" then
				expect(reference.target).to.equal(camera)
				found = true
			end
		end
		expect(found).to.equal(true)
		camera:Destroy()
	end)

	it("encodes live scalar service properties and reports unreadable ones", function()
		const warnings = {}
		const properties = StudioExport.serviceProperties({
			Name = "Lighting",
			ClassName = "Lighting",
			Brightness = 7,
			GeographicLatitude = 38,
			GetAttributes = function()
				return {}
			end,
		}, warnings)
		expect(properties.Name.String).to.equal("Lighting")
		expect(properties.Brightness.Float32).to.equal(7)
		expect(properties.GeographicLatitude.Float32).to.equal(38)
		expect(properties.AttributesSerialize).to.equal(nil)
		expect(type(properties.Attributes.Attributes)).to.equal("table")
		expect(#warnings > 0).to.equal(true)
	end)

	it("captures readable read-only service properties without requiring write access", function()
		const properties = StudioExport.serviceProperties({
			Name = "HttpService",
			ClassName = "HttpService",
			HttpEnabled = true,
			GetAttributes = function()
				return {}
			end,
		}, {})
		expect(properties.HttpEnabled.Bool).to.equal(true)
	end)

	it("captures supported service getters when reflection marks them inaccessible", function()
		const properties = StudioExport.serviceProperties({
			Name = "Workspace",
			ClassName = "Workspace",
			StreamOutBehavior = Enum.StreamOutBehavior.Opportunistic,
			StreamingIntegrityMode = Enum.StreamingIntegrityMode.PauseOutsideLoadedArea,
			StreamingTargetRadius = 512,
			GetAttributes = function()
				return {}
			end,
		}, {})
		expect(properties.StreamOutBehavior.Enum).to.equal(Enum.StreamOutBehavior.Opportunistic.Value)
		expect(properties.StreamingIntegrityMode.Enum).to.equal(
			Enum.StreamingIntegrityMode.PauseOutsideLoadedArea.Value
		)
		expect(properties.StreamingTargetRadius.Int32).to.equal(512)
	end)

	it("captures new service enums directly without translating obsolete reflection enums", function()
		for _, case in
			{
				{ ClassName = "Workspace", Property = "NextGenerationPhysicsSolver", Enum = Enum.RolloutState },
				{ ClassName = "Lighting", Property = "LightAttenuationMode", Enum = Enum.LightAttenuationMode },
			}
		do
			for _, value in case.Enum:GetEnumItems() do
				const instance = {
					ClassName = case.ClassName,
					Name = case.ClassName,
					[case.Property] = value,
					GetAttributes = function()
						return {}
					end,
				}
				const properties = StudioExport.serviceProperties(instance, {})
				expect(properties[case.Property].Enum).to.equal(value.Value)
			end
		end
	end)

	it("captures reflection-missing Workspace Content and enums from each current getter", function()
		const fields = { "ExplorerIconContent", "ReplicationFrequency", "ReplicationThroughput" }
		for _, name in fields do
			expect(RbxDom.findCanonicalPropertyDescriptor("Workspace", name)).to.equal(nil)
		end
		expect(Enum.ReplicationMode.Automatic.Value).to.equal(0)
		const values = {}
		const instance = setmetatable({
			ClassName = "Workspace",
			Name = "Workspace",
			GetAttributes = function()
				return {}
			end,
		}, {
			__index = function(_, name)
				return values[name]
			end,
		})
		for _, content in { Content.none, Content.fromUri("rbxassetid://123"), Content.none } do
			for _, frequency in Enum.ReplicationMode:GetEnumItems() do
				for _, throughput in Enum.ReplicationMode:GetEnumItems() do
					values.ExplorerIconContent = content
					values.ReplicationFrequency = frequency
					values.ReplicationThroughput = throughput
					const warnings = {}
					const properties = StudioExport.serviceProperties(instance, warnings)
					if content == Content.none then
						expect(properties.ExplorerIconContent.Content).to.equal("None")
					else
						expect(properties.ExplorerIconContent.Content.Uri).to.equal(content.Uri)
					end
					expect(properties.ReplicationFrequency.Enum).to.equal(frequency.Value)
					expect(properties.ReplicationThroughput.Enum).to.equal(throughput.Value)
					for _, warning in warnings do
						for _, name in fields do
							expect(warning:find(`"Workspace.{name}"`, 1, true)).to.equal(nil)
						end
					end
				end
			end
		end
	end)

	it("reports missing and denied supplemental Workspace getters without inventing values", function()
		for _, name in { "ExplorerIconContent", "ReplicationFrequency", "ReplicationThroughput" } do
			for _, reason in { "Invalid member", "Permission denied" } do
				const warnings = {}
				const instance = setmetatable({
					ClassName = "Workspace",
					Name = "Workspace",
					GetAttributes = function()
						return {}
					end,
				}, {
					__index = function(_, property)
						if property == name then
							error(`{reason} for {name}.`)
						end
						return nil
					end,
				})
				const properties = StudioExport.serviceProperties(instance, warnings)
				expect(properties[name]).to.equal(nil)
				local reported = 0
				for _, warning in warnings do
					if warning:find(`Omitted unreadable service property "Workspace.{name}".`, 1, true) then
						expect(warning:find(`{reason} for {name}.`, 1, true) ~= nil).to.equal(true)
						reported += 1
					end
				end
				expect(reported).to.equal(1)
			end
		end
	end)

	it("reports invalid supplemental Workspace values through the existing encoder warnings", function()
		const warnings = {}
		const properties = StudioExport.serviceProperties({
			ClassName = "Workspace",
			ExplorerIconContent = true,
			Name = "Workspace",
			ReplicationFrequency = true,
			ReplicationThroughput = true,
			GetAttributes = function()
				return {}
			end,
		}, warnings)
		for _, name in { "ExplorerIconContent", "ReplicationFrequency", "ReplicationThroughput" } do
			expect(properties[name]).to.equal(nil)
			local reported = 0
			for _, warning in warnings do
				if warning:find(`Omitted unsupported service property "Workspace.{name}".`, 1, true) then
					reported += 1
				end
			end
			expect(reported).to.equal(1)
		end
	end)

	it("reports failed getters for new service enums without synthesizing values", function()
		for _, case in
			{
				{ ClassName = "Workspace", Property = "NextGenerationPhysicsSolver" },
				{ ClassName = "Lighting", Property = "LightAttenuationMode" },
			}
		do
			const warnings = {}
			const instance = setmetatable({
				ClassName = case.ClassName,
				Name = case.ClassName,
				GetAttributes = function()
					return {}
				end,
			}, {
				__index = function(_, name)
					if name == case.Property then
						error(`New getter unavailable for {name}.`)
					end
					return nil
				end,
			})
			const properties = StudioExport.serviceProperties(instance, warnings)
			expect(properties[case.Property]).to.equal(nil)
			expect(table.concat(warnings, "\n"):find(`New getter unavailable for {case.Property}.`, 1, true) ~= nil).to.equal(
				true
			)
		end
	end)

	it("captures material overrides through the public getter without reading hidden name fields", function()
		const queried = {}
		local hiddenReads = 0
		const instance = setmetatable({
			ClassName = "MaterialService",
			Name = "MaterialService",
			GetAttributes = function()
				return {}
			end,
			GetBaseMaterialOverride = function(_, material)
				expect(material.EnumType).to.equal(Enum.Material)
				expect(queried[material]).to.equal(nil)
				queried[material] = true
				return if material == Enum.Material.Rock then "" else `Custom-{material.Name}`
			end,
		}, {
			__index = function(_, name)
				if name == "AsphaltName" or name == "GrassName" or name == "RockName" then
					hiddenReads += 1
					error("Hidden material names must not be read.")
				end
				return nil
			end,
		})
		const properties = StudioExport.serviceProperties(instance, {})
		expect(hiddenReads).to.equal(0)
		expect(properties.AsphaltName.String).to.equal("Custom-Asphalt")
		expect(properties.GrassName.String).to.equal("Custom-Grass")
		expect(properties.RockName.String).to.equal("")
		expect(queried[Enum.Material.Asphalt]).to.equal(true)
		expect(queried[Enum.Material.Grass]).to.equal(true)
		expect(queried[Enum.Material.Rock]).to.equal(true)
		expect(queried[Enum.Material.Air]).to.equal(nil)
	end)

	it("reports failed public material getters without inventing override names", function()
		const warnings = {}
		const properties = StudioExport.serviceProperties({
			ClassName = "MaterialService",
			Name = "MaterialService",
			GetAttributes = function()
				return {}
			end,
			GetBaseMaterialOverride = function(_, material)
				error(`Public material getter failed for {material.Name}.`)
			end,
		}, warnings)
		expect(properties.GrassName).to.equal(nil)
		const text = table.concat(warnings, "\n")
		expect(text:find('Omitted unreadable service property "MaterialService.GrassName"', 1, true) ~= nil).to.equal(
			true
		)
		expect(text:find("Public material getter failed for Grass.", 1, true) ~= nil).to.equal(true)
	end)

	it("captures collision groups through public APIs without reading the hidden property", function()
		const physics = {
			GetRegisteredCollisionGroups = function()
				return { { name = "Later", mask = -4 }, { name = "Default", mask = -1 } }
			end,
			GetCollisionGroupId = function(_, name)
				return if name == "Later" then 7 else 0
			end,
		}
		const instance = setmetatable({
			ClassName = "Workspace",
			Name = "Workspace",
			Parent = {
				FindFirstChildOfClass = function(_, class)
					expect(class).to.equal("PhysicsService")
					return physics
				end,
			},
			GetAttributes = function()
				return {}
			end,
		}, {
			__index = function(_, name)
				assert(name ~= "CollisionGroupData", "Hidden collision bytes must not be read.")
				return nil
			end,
		})
		const properties = StudioExport.serviceProperties(instance, {})
		const ok, expected = RbxDom.EncodedValue.encode(
			string.char(1, 2, 0, 4, 255, 255, 255, 255, 7)
				.. "Default"
				.. string.char(7, 4, 252, 255, 255, 255, 5)
				.. "Later",
			"BinaryString"
		)
		expect(ok).to.equal(true)
		expect(properties.CollisionGroupData.BinaryString).to.equal(expected.BinaryString)
	end)

	it("reports denied service getters without inventing a property value", function()
		const instance = setmetatable({
			Name = "HttpService",
			ClassName = "HttpService",
			GetAttributes = function()
				return {}
			end,
		}, {
			__index = function(_, name)
				if name == "HttpEnabled" then
					error("The caller cannot read this property.")
				end
				return nil
			end,
		})
		const warnings = {}
		const properties = StudioExport.serviceProperties(instance, warnings)
		expect(properties.HttpEnabled).to.equal(nil)
		local reported = false
		for _, warning in warnings do
			if string.find(warning, 'unreadable service property "HttpService.HttpEnabled"', 1, true) then
				reported = true
			end
		end
		expect(reported).to.equal(true)
	end)

	it("reports the restore impact of unavailable Workspace pivot migration state without guessing it", function()
		for _, state in { "denied", false, true } do
			const warnings = {}
			const instance = setmetatable({
				Name = "Workspace",
				ClassName = "Workspace",
				GetAttributes = function()
					return {}
				end,
			}, {
				__index = function(_, name)
					if name == "NeedsPivotMigration" then
						if state == "denied" then
							error("The caller cannot read pivot migration state.")
						end
						return state
					end
					return nil
				end,
			})
			const properties = StudioExport.serviceProperties(instance, warnings)
			const warned = table.concat(warnings, "\n"):find("legacy non-descendant PrimaryPart references", 1, true)
				~= nil
			if state == "denied" then
				expect(properties.NeedsPivotMigration).to.equal(nil)
				expect(warned).to.equal(true)
			else
				expect(properties.NeedsPivotMigration.Bool).to.equal(state)
				expect(warned).to.equal(false)
			end
		end
	end)

	it("distinguishes captured null service references from denied getters", function()
		for _, denied in { false, true } do
			const instance = setmetatable({
				Name = "StarterGui",
				ClassName = "StarterGui",
				GetAttributes = function()
					return {}
				end,
			}, {
				__index = function(_, name)
					if name == "StudioDefaultStyleSheet" and denied then
						error("The caller cannot read this reference.")
					end
					return nil
				end,
			})
			const properties = StudioExport.serviceProperties(instance, {})
			if denied then
				expect(properties.StudioDefaultStyleSheet).to.equal(nil)
			else
				expect(properties.StudioDefaultStyleSheet.Ref).to.equal("00000000000000000000000000000000")
			end
		end
	end)

	it("keeps encodable attributes while reporting unsupported entries", function()
		const warnings = {}
		const properties = StudioExport.serviceProperties({
			Name = "Lighting",
			ClassName = "Lighting",
			GetAttributes = function()
				return { Authored = "preserved", Unsupported = function() end }
			end,
		}, warnings)
		expect(properties.Attributes.Attributes.Authored.String).to.equal("preserved")
		expect(properties.Attributes.Attributes.Unsupported).to.equal(nil)
		local reported = false
		for _, warning in warnings do
			if string.find(warning, 'unsupported service attribute "Lighting.Unsupported"', 1, true) then
				reported = true
			end
		end
		expect(reported).to.equal(true)
	end)

	it("preserves engine attributes in a backup without changing the source table", function()
		const attributes = {
			Authored = "preserved",
			RBX_LightingTechnologyUnifiedMigration = true,
			RBX_OriginalTechnologyOnFileLoad = 3,
		}
		const properties = StudioExport.serviceProperties({
			Name = "Lighting",
			ClassName = "Lighting",
			GetAttributes = function()
				return attributes
			end,
		}, {})
		const encoded = properties.Attributes.Attributes
		expect(encoded.RBX_OriginalTechnologyOnFileLoad.Float64).to.equal(3)
		expect(encoded.RBX_LightingTechnologyUnifiedMigration.Bool).to.equal(true)
		expect(encoded.Authored.String).to.equal("preserved")
		expect(attributes.RBX_OriginalTechnologyOnFileLoad).to.equal(3)
		expect(attributes.RBX_LightingTechnologyUnifiedMigration).to.equal(true)
	end)

	it("uploads bounded binary parts and a msgpack manifest in order", function()
		const root = fixture()
		const boundary = boundaries(root)
		const contents = string.rep("\0", 1048576) .. "tail"
		boundary.serialize = function()
			return contents
		end
		const calls = {}
		boundary.post = function(url, body)
			table.insert(calls, { url = url, body = body })
			return Promise.resolve({ code = 200 })
		end
		const ok = StudioExport.run({
			getBaseUrl = function()
				return "http://localhost:1"
			end,
		}, request(), boundary):await()
		expect(ok).to.equal(true)
		expect(#calls).to.equal(3)
		expect(calls[1].url).to.equal("http://localhost:1/api/export/part/test-job/0")
		expect(#calls[1].body).to.equal(1048576)
		expect(calls[2].body).to.equal("tail")
		const manifest = Http.msgpackDecode(calls[3].body)
		expect(manifest.partCount).to.equal(2)
		expect(#manifest.roots).to.equal(1)
		expect(#manifest.services).to.equal(0)
		root:Destroy()
	end)

	it("reports capture and HTTP failures through an empty failure manifest", function()
		for _, failure in { "capture", "http", "transport", "finish" } do
			const root = fixture()
			const boundary = boundaries(root)
			if failure == "capture" then
				boundary.serialize = function()
					error("Capture failed.")
				end
			end
			const calls = {}
			boundary.post = function(url, bytes)
				table.insert(calls, { url = url, body = bytes })
				if #calls == 1 and failure == "transport" then
					return Promise.reject("Transport failed.")
				end
				return Promise.resolve({
					code = if (#calls == 1 and failure == "http") or (#calls == 2 and failure == "finish")
						then 500
						else 200,
				})
			end
			const ok = StudioExport.run({
				getBaseUrl = function()
					return "http://localhost:1"
				end,
			}, request(), boundary):await()
			expect(ok).to.equal(false)
			const manifest = Http.msgpackDecode(calls[#calls].body)
			expect(manifest.partCount).to.equal(0)
			expect(#manifest.roots).to.equal(0)
			expect(#manifest.services).to.equal(0)
			expect(#manifest.warnings).to.equal(0)
			expect(type(manifest.error)).to.equal("string")
			root:Destroy()
		end
	end)

	it("accepts the export wire packet and rejects invalid part sizes", function()
		const body = request()
		expect(Types.ApiSocketPacket({ sessionId = "session", packetType = "export", body = body })).to.equal(true)
		body.partSize = 0
		expect(Types.ExportPacket(body)).to.equal(false)
	end)
end
