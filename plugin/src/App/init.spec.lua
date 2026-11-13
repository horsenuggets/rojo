return function()
	local App = require(script.Parent)({}).component

	it("should not show sync reminders for the fork", function()
		expect(App.checkSyncReminder({})).to.equal(nil)
		expect(App.startSyncReminderPolling({})).to.equal(nil)
		expect(App.sendSyncReminder({}, "Test", {})).to.equal(nil)
	end)
end
