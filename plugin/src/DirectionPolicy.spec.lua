--[[

DirectionPolicy.spec

Exercises the negotiation matrix without Studio or persistent settings.

--]]

return function()
	const Policy = require("./DirectionPolicy")
	const directions = { "fs-to-studio", "studio-to-fs", "bidirectional" }
	const function info(direction)
		return { sessionId = "session", syncDirection = direction, supportedSyncDirections = table.clone(directions) }
	end
	const function marker(direction)
		return Policy.marker("localhost:34872", "session", direction)
	end

	for _, enabled in { false, true } do
		for _, direction in directions do
			it(`pins {direction} independently of the two-way setting {enabled}`, function()
				expect(Policy.negotiate(info(direction), marker(direction), enabled)).to.equal(direction)
			end)
		end
		it(`preserves settings without an explicit policy with two-way {enabled}`, function()
			expect(Policy.negotiate({}, nil, enabled)).to.equal(if enabled then "bidirectional" else "fs-to-studio")
			expect(Policy.negotiate({}, Policy.marker("localhost:34872"), enabled)).to.equal("bidirectional")
		end)
	end

	it("validates optional server policy regardless of debug typechecking", function()
		for _, invalid in { false, 1, "Both", {}, "" } do
			expect(function()
				Policy.validateServer({ syncDirection = invalid, supportedSyncDirections = directions })
			end).to.throw()
		end
		for _, supported in
			{ false, "bidirectional", {}, { "bidirectional" }, { "fs-to-studio", "studio-to-fs", "studio-to-fs" } }
		do
			expect(function()
				Policy.validateServer({ supportedSyncDirections = supported })
			end).to.throw()
		end
		expect(function()
			Policy.validateServer({ syncDirection = "bidirectional" })
		end).to.throw()
	end)

	it("ignores directions added by newer servers but still requires the known three", function()
		const extended = table.clone(directions)
		table.insert(extended, "future-direction")
		Policy.validateServer({ supportedSyncDirections = extended })
		expect(Policy.negotiate({
			sessionId = "session",
			syncDirection = "bidirectional",
			supportedSyncDirections = extended,
		}, marker("bidirectional"), false)).to.equal("bidirectional")
		for _, malformed in { { "fs-to-studio", "studio-to-fs", "bidirectional", 4 }, { key = "bidirectional" } } do
			expect(function()
				Policy.validateServer({ supportedSyncDirections = malformed })
			end).to.throw()
		end
	end)

	it("rejects incomplete and malformed generated-place markers", function()
		for _, args in
			{
				{ false },
				{ "localhost" },
				{ "localhost:0" },
				{ "localhost:65536" },
				{ "localhost:34872", "session" },
				{ "localhost:34872", "", "bidirectional" },
				{ "localhost:34872", "session", "invalid" },
			}
		do
			expect(function()
				Policy.marker(table.unpack(args))
			end).to.throw()
		end
		expect(function()
			Policy.marker(nil, "session", "bidirectional")
		end).to.throw()
		expect(function()
			Policy.marker("localhost:34872", nil, "bidirectional")
		end).to.throw()
	end)

	it("refuses port reuse, changed direction and unseeded Studio-only takeover", function()
		const reused = info("studio-to-fs")
		reused.sessionId = "different-session"
		expect(function()
			Policy.negotiate(reused, marker("studio-to-fs"), true)
		end).to.throw()
		expect(function()
			Policy.negotiate(info("fs-to-studio"), marker("bidirectional"), true)
		end).to.throw()
		expect(function()
			Policy.negotiate(info("studio-to-fs"), nil, true)
		end).to.throw()
		expect(function()
			Policy.negotiate(info("studio-to-fs"), Policy.marker("localhost:34872"), true)
		end).to.throw()
		expect(function()
			Policy.negotiate({}, marker("studio-to-fs"), true)
		end).to.throw()
	end)

	for _, direction in directions do
		for _, origin in { "filesystem", "studio", "missing" } do
			it(`filters {origin} provenance under {direction}`, function()
				if direction == "studio-to-fs" and origin == "missing" then
					expect(function()
						Policy.allowsPublication(direction, nil)
					end).to.throw()
				else
					expect(Policy.allowsPublication(direction, if origin == "missing" then nil else origin)).to.equal(
						direction ~= "studio-to-fs" or origin == "studio"
					)
				end
			end)
		end
	end

	it("keys identity evidence by exact endpoint session and direction", function()
		const key = Policy.resumeKey("http://localhost:34872", "session", "studio-to-fs")
		for _, other in
			{
				Policy.resumeKey("https://localhost:34872", "session", "studio-to-fs"),
				Policy.resumeKey("http://localhost:34872", "other", "studio-to-fs"),
				Policy.resumeKey("http://localhost:34872", "session", "bidirectional"),
			}
		do
			expect(other == key).to.equal(false)
		end
	end)
end
