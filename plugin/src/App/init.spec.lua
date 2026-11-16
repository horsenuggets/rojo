return function()
	local App = require(script.Parent)({}).component

	it("should not show sync reminders for the fork", function()
		expect(App.checkSyncReminder({})).to.equal(nil)
		expect(App.startSyncReminderPolling({})).to.equal(nil)
		expect(App.sendSyncReminder({}, "Test", {})).to.equal(nil)
	end)

	-- Only the session the place was built for gets the live edit behavior, so a
	-- manual session to another project keeps the user's settings and confirmation.
	it("should only treat the marked endpoint as live edit", function()
		local marked = {
			liveEditConnection = {
				host = "localhost",
				port = "34901",
			},
		}
		expect(App.isLiveEditEndpoint(marked, "localhost", "34901")).to.equal(true)
		expect(App.isLiveEditEndpoint(marked, "localhost", 34901)).to.equal(true)
		expect(App.isLiveEditEndpoint(marked, "localhost", "34872")).to.equal(false)
		expect(App.isLiveEditEndpoint(marked, "127.0.0.1", "34901")).to.equal(false)
		expect(App.isLiveEditEndpoint({}, "localhost", "34901")).to.equal(false)
	end)
end
