return function()
	local StudioSyncback = require(script.Parent.StudioSyncback)

	local function makeService(className, children)
		local service = Instance.new("Folder")
		service.Name = className
		for _, child in children do
			child.Parent = service
		end
		return service
	end

	local function named(className, name, archivable)
		local instance = Instance.new(className)
		instance.Name = name
		instance.Archivable = archivable ~= false
		return instance
	end

	describe("collect", function()
		it("should list archivable children in service order with a matching manifest", function()
			local storage = makeService("ReplicatedStorage", {
				named("ModuleScript", "Shared"),
				named("Folder", "Temporary", false),
			})
			local workspace = makeService("Workspace", { named("Part", "Target") })
			local places = { ReplicatedStorage = storage, Workspace = workspace }

			local instances, manifest, missing = StudioSyncback.collect({
				{ name = "ReplicatedStorage", className = "ReplicatedStorage" },
				{ name = "Lighting", className = "Lighting" },
				{ name = "Workspace", className = "Workspace" },
			}, function(className)
				return places[className]
			end)

			expect(#instances).to.equal(2)
			expect(instances[1].Name).to.equal("Shared")
			expect(instances[2].Name).to.equal("Target")

			expect(#manifest).to.equal(2)
			expect(manifest[1].service).to.equal("ReplicatedStorage")
			expect(manifest[1].name).to.equal("Shared")
			expect(manifest[1].className).to.equal("ModuleScript")
			expect(manifest[2].service).to.equal("Workspace")
			expect(manifest[2].className).to.equal("Part")

			expect(#missing).to.equal(1)
			expect(missing[1]).to.equal("Lighting")

			storage:Destroy()
			workspace:Destroy()
		end)

		it("should report the zero-based manifest index of the current camera", function()
			local camera = named("Camera", "Camera")
			local service = makeService("Workspace", { named("Part", "A"), camera })
			local services = { { name = "Workspace", className = "Workspace" } }
			local function find()
				return service
			end

			local _, _, _, cameraIndex = StudioSyncback.collect(services, find, camera)
			expect(cameraIndex).to.equal(1)

			local _, _, _, noCamera = StudioSyncback.collect(services, find, nil)
			expect(noCamera).to.equal(nil)

			camera.Archivable = false
			local _, _, _, unarchivable = StudioSyncback.collect(services, find, camera)
			expect(unarchivable).to.equal(nil)

			service:Destroy()
		end)

		it("should keep the manifest and instances the same length", function()
			local service = makeService("Workspace", {
				named("Part", "A"),
				named("Part", "B", false),
				named("Part", "C"),
			})
			local instances, manifest = StudioSyncback.collect({
				{ name = "Workspace", className = "Workspace" },
			}, function()
				return service
			end)

			expect(#instances).to.equal(#manifest)
			for index, instance in instances do
				expect(manifest[index].name).to.equal(instance.Name)
			end

			service:Destroy()
		end)
	end)

	describe("partRanges", function()
		it("should split into full parts and a shorter last part", function()
			local ranges = StudioSyncback.partRanges(10, 4)
			expect(#ranges).to.equal(3)
			expect(ranges[1][1]).to.equal(1)
			expect(ranges[1][2]).to.equal(4)
			expect(ranges[3][1]).to.equal(9)
			expect(ranges[3][2]).to.equal(10)
		end)

		it("should return no parts for no bytes", function()
			expect(#StudioSyncback.partRanges(0, 4)).to.equal(0)
		end)

		it("should cover every byte exactly once for any size", function()
			for size = 1, 40 do
				for partSize = 1, 12 do
					local ranges = StudioSyncback.partRanges(size, partSize)
					local nextByte = 1
					for _, range in ranges do
						expect(range[1]).to.equal(nextByte)
						expect(range[2] - range[1] + 1 <= partSize).to.equal(true)
						expect(range[2] >= range[1]).to.equal(true)
						nextByte = range[2] + 1
					end
					expect(nextByte).to.equal(size + 1)
					expect(#ranges).to.equal(math.ceil(size / partSize))
				end
			end
		end)

		it("should be pure", function()
			local first = StudioSyncback.partRanges(25, 7)
			local second = StudioSyncback.partRanges(25, 7)
			expect(#first).to.equal(#second)
			for index, range in first do
				expect(range[1]).to.equal(second[index][1])
				expect(range[2]).to.equal(second[index][2])
			end
		end)
	end)

	describe("describeError", function()
		local function responseWith(body, decodes)
			return {
				msgpack = function()
					if not decodes then
						error("not msgpack")
					end
					return body
				end,
			}
		end

		it("should use the server's details when the body has them", function()
			local err = { response = responseWith({ details = "The upload is missing parts." }, true) }
			expect(StudioSyncback.describeError(err)).to.equal("The upload is missing parts.")
		end)

		it("should fall back to the error itself otherwise", function()
			expect(StudioSyncback.describeError("plain message")).to.equal("plain message")

			local undecodable = setmetatable({ response = responseWith(nil, false) }, {
				__tostring = function()
					return "raw error"
				end,
			})
			expect(StudioSyncback.describeError(undecodable)).to.equal("raw error")

			local noDetails = setmetatable({ response = responseWith({ kind = "BadRequest" }, true) }, {
				__tostring = function()
					return "raw error"
				end,
			})
			expect(StudioSyncback.describeError(noDetails)).to.equal("raw error")
		end)
	end)
end
