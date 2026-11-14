return function()
	local Error = require(script.Parent.Error)

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
end
