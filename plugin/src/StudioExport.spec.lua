return function()
	const StudioExport = require(script.Parent.StudioExport)
	const Packages = script.Parent.Parent.Packages
	const Http = require(Packages.Http)
	const Promise = require(Packages.Promise)
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
		expect(#manifest.warnings).to.equal(2)
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
