return function()
	local SharedAPI = require(script.Parent.SharedAPI)

	it("reports the effective session direction rather than persistent settings", function()
		for _, direction in { "fs-to-studio", "studio-to-fs", "bidirectional" } do
			local status = SharedAPI.describeStatus({
				state = { appStatus = "Connected", projectName = "Project", address = "localhost" },
				serveSession = {
					getSyncDirection = function()
						return direction
					end,
				},
			})
			expect(status.syncDirection).to.equal(direction)
			expect(status.projectName).to.equal("Project")
			expect(status.address).to.equal("localhost")
		end
		expect(SharedAPI.describeStatus(nil).status).to.equal("Unloaded")
		local failed = SharedAPI.describeStatus({ state = { appStatus = "Error", errorMessage = "Retained drafts." } })
		expect(failed.error).to.equal("Retained drafts.")
		expect(failed.syncDirection).to.equal(nil)
	end)

	it("does not let standalone shared syncback bypass a filesystem-only session", function()
		for _, direction in { "fs-to-studio", "studio-to-fs", "bidirectional" } do
			expect(SharedAPI.canSyncbackFromStudio({
				serveSession = {
					getSyncDirection = function()
						return direction
					end,
				},
			})).to.equal(direction ~= "fs-to-studio")
		end
		expect(SharedAPI.canSyncbackFromStudio({})).to.equal(true)
	end)

	it("should only configure two-way sync without an active session", function()
		local expected = {
			Confirming = false,
			Connected = false,
			Connecting = false,
			Error = true,
			NotConnected = true,
			Settings = true,
		}

		for appStatus, canConfigure in expected do
			expect(SharedAPI.canConfigureTwoWaySync(appStatus)).to.equal(canConfigure)
		end
	end)

	it("should only accept the two write authorities", function()
		local cases = {
			{ "Filesystem", true },
			{ "Studio", true },
			{ "filesystem", false },
			{ "studio", false },
			{ "", false },
			{ true, false },
			{ nil, false },
		}
		for _, case in cases do
			expect(SharedAPI.isWriteAuthority(case[1])).to.equal(case[2])
		end
	end)

	-- These use a plain table in place of `shared`, so the live plugin's API is
	-- never touched.
	describe("shared names", function()
		it("should take shared.Rojo only when it's free", function()
			local ours, other = {}, {}

			local free = {}
			SharedAPI.claimGlobals(free, ours)
			expect(free.RojoPlus).to.equal(ours)
			expect(free.Rojo).to.equal(ours)

			-- Another Rojo build loaded first.
			local taken = { Rojo = other }
			SharedAPI.claimGlobals(taken, ours)
			expect(taken.RojoPlus).to.equal(ours)
			expect(taken.Rojo).to.equal(other)
		end)

		it("should never clear another build's API in either load order", function()
			local ours, other = {}, {}
			for _, otherFirst in { true, false } do
				local globals = {}
				if otherFirst then
					globals.Rojo = other
				end
				SharedAPI.claimGlobals(globals, ours)
				if not otherFirst then
					-- Another build loads later and takes shared.Rojo over.
					globals.Rojo = other
				end

				SharedAPI.releaseGlobals(globals, ours)
				expect(globals.RojoPlus).to.equal(nil)
				expect(globals.Rojo).to.equal(other)
			end
		end)

		it("should clear both names when Rojo+ held them", function()
			local ours = {}
			local globals = {}
			SharedAPI.claimGlobals(globals, ours)
			SharedAPI.releaseGlobals(globals, ours)
			expect(globals.RojoPlus).to.equal(nil)
			expect(globals.Rojo).to.equal(nil)

			-- Releasing without ever registering changes nothing.
			local untouched = { Rojo = ours }
			SharedAPI.releaseGlobals(untouched, nil)
			expect(untouched.Rojo).to.equal(ours)
		end)
	end)
end
