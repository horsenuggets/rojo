return function()
	local SharedAPI = require(script.Parent.SharedAPI)

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

	it("should never clear another Rojo build's shared API on unload", function()
		-- Registration only happens in Edit mode.
		if not game:GetService("RunService"):IsEdit() then
			return
		end

		local previous = {}
		for _, name in SharedAPI.GLOBAL_NAMES do
			previous[name] = shared[name]
		end

		SharedAPI.register({ state = {} })
		local ours = shared.RojoPlus
		expect(ours).to.be.ok()
		expect(shared.Rojo).to.equal(ours)

		-- Another Rojo build claims shared.Rojo after Rojo+ loaded.
		local other = {}
		shared.Rojo = other
		SharedAPI.unregister()

		expect(shared.RojoPlus).to.equal(nil)
		expect(shared.Rojo).to.equal(other)

		for _, name in SharedAPI.GLOBAL_NAMES do
			shared[name] = previous[name]
		end
	end)
end
