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

	-- A bare host and its "http://" spelling reach the same server, in either
	-- the marker or the session. HTTPS is a different endpoint.
	it("should treat an http prefix as the same live edit host", function()
		for _, markerHost in { "localhost", "http://localhost" } do
			local marked = {
				liveEditConnection = {
					host = markerHost,
					port = "34901",
				},
			}
			expect(App.isLiveEditEndpoint(marked, "localhost", "34901")).to.equal(true)
			expect(App.isLiveEditEndpoint(marked, "http://localhost", "34901")).to.equal(true)
			expect(App.isLiveEditEndpoint(marked, "https://localhost", "34901")).to.equal(false)
			expect(App.isLiveEditEndpoint(marked, "http://127.0.0.1", "34901")).to.equal(false)
			expect(App.isLiveEditEndpoint(marked, "http://localhost", "34872")).to.equal(false)
		end
	end)
end
