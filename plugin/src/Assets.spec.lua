return function()
	local Assets = require(script.Parent.Assets)

	it("should play a notification sound that loads", function()
		local ContentProvider = game:GetService("ContentProvider")
		local sound = Instance.new("Sound")
		sound.SoundId = Assets.Sounds.Notification
		local failed = false
		ContentProvider:PreloadAsync({ sound }, function(_, status)
			failed = status ~= Enum.AssetFetchStatus.Success
		end)
		sound:Destroy()
		expect(failed).to.equal(false)
	end)

	it("should use the fork logo for every branded image", function()
		for _, name in { "Logo", "PluginButton", "PluginButtonConnected", "PluginButtonWarning" } do
			expect(Assets.Images[name]).to.equal("rbxassetid://70875160573040")
		end
	end)
end
