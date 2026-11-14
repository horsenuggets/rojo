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
			}, function(name, className)
				local found = places[name]
				return if found and className == name then found else nil
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

	describe("cameraPath", function()
		local function expectPath(path, expected)
			expect(path).to.be.ok()
			expect(#path).to.equal(#expected)
			for index, value in expected do
				expect(path[index]).to.equal(value)
			end
		end

		it("should point at an uploaded camera by its upload index", function()
			local part, camera = named("Part", "A"), named("Camera", "Camera")
			expectPath(StudioSyncback.cameraPath({ part, camera }, camera), { 1 })
		end)

		it("should find a nested camera, counting only archivable siblings", function()
			local folder = named("Folder", "Cameras")
			named("Part", "Skipped", false).Parent = folder
			named("Part", "Kept").Parent = folder
			local camera = named("Camera", "Nested")
			camera.Parent = folder

			expectPath(StudioSyncback.cameraPath({ named("Part", "First"), folder }, camera), { 1, 1 })
			folder:Destroy()
		end)

		it("should be nil when the camera isn't part of the upload", function()
			local folder = named("Folder", "Hidden", false)
			local camera = named("Camera", "Camera")
			camera.Parent = folder
			local uploaded = named("Folder", "Uploaded")

			expect(StudioSyncback.cameraPath({ uploaded }, nil)).to.equal(nil)
			expect(StudioSyncback.cameraPath({ uploaded }, camera)).to.equal(nil)
			expect(StudioSyncback.cameraPath({ folder }, camera)).to.equal(nil)

			camera.Parent = uploaded
			camera.Archivable = false
			expect(StudioSyncback.cameraPath({ uploaded }, camera)).to.equal(nil)

			folder:Destroy()
			uploaded:Destroy()
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
		it("should use the server's details when the error has them", function()
			local err = { details = "The upload is missing parts." }
			expect(StudioSyncback.describeError(err)).to.equal("The upload is missing parts.")
		end)

		it("should fall back to the error itself otherwise", function()
			expect(StudioSyncback.describeError("plain message")).to.equal("plain message")

			local noDetails = setmetatable({}, {
				__tostring = function()
					return "raw error"
				end,
			})
			expect(StudioSyncback.describeError(noDetails)).to.equal("raw error")
		end)
	end)

	describe("baseUrl", function()
		it("should bracket IPv6 addresses only", function()
			local cases = {
				{ "localhost", "http://localhost:34872" },
				{ "127.0.0.1", "http://127.0.0.1:34872" },
				{ "fd00::1", "http://[fd00::1]:34872" },
				{ "::1", "http://[::1]:34872" },
				{ "[::1]", "http://[::1]:34872" },
			}
			for _, case in cases do
				expect(StudioSyncback.baseUrl(case[1], "34872")).to.equal(case[2])
			end
		end)
	end)

	describe("describeMissing", function()
		it("should name every missing mapping with the right grammar", function()
			expect(StudioSyncback.describeMissing({ "Lighting" })).to.equal(
				'The project maps "Lighting", but this place doesn\'t have it, so nothing was sent. '
					.. "Syncing back would remove its files."
			)
			expect(StudioSyncback.describeMissing({ "Lighting", "Shared" })).to.equal(
				'The project maps "Lighting", "Shared", but this place doesn\'t have them, so nothing '
					.. "was sent. Syncing back would remove their files."
			)
		end)
	end)
end
