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
end
