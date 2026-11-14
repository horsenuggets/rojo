return function()
	local Error = require(script.Parent.Error)
	local msgpack = require(script.Parent.Parent.msgpack)

	it("should keep the response of a failed request", function()
		local response = { code = 400, body = "details" }
		local err = Error.fromResponse(response)

		expect(err.type).to.equal(Error.Kind.Unknown)
		expect(err.response).to.equal(response)
	end)

	it("should treat timeouts as timeouts", function()
		local err = Error.fromResponse({ code = 504, body = "" })
		expect(err.type).to.equal(Error.Kind.Timeout)
	end)

	it("should show the details of a Rojo error body", function()
		local body = msgpack.encode({ kind = "BadRequest", details = "Only 100% of parts arrived." })
		local err = Error.fromResponse({ code = 400, body = body })

		expect(err.details).to.equal("Only 100% of parts arrived.")
		expect(tostring(err)).to.equal("Unknown HTTP error: 400: Only 100% of parts arrived.")
	end)

	it("should fall back to the raw body when it isn't a Rojo error", function()
		for _, body in { "plain text", "", msgpack.encode({ kind = "BadRequest" }) } do
			local err = Error.fromResponse({ code = 400, body = body })
			expect(err.details).to.equal(nil)
			expect(tostring(err)).to.equal("Unknown HTTP error: 400: " .. body)
		end
	end)
end
