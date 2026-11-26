--[[

Settings.spec

Checks notification sound defaults independently of saved plugin preferences.

--]]

const Settings = require(script.Parent.Settings)

return function()
	it("starts notification sounds disabled", function()
		expect(Settings._defaults.playSounds).to.equal(false)
	end)
end
