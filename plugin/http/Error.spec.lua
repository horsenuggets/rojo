return function()
	local Error = require(script.Parent.Error)
	local msgpack = require(script.Parent.Parent.msgpack)

	it("should keep the response of a failed request", function()
		local response = { code = 400, body = "details" }
		local err = Error.fromResponse(response)

		expect(err.type).to.equal(Error.Kind.Unknown)
		expect(err.response).to.equal(response)
	end)

	for _, code in { 403, 408, 504 } do
		it(`should retain HTTP {code} timeout response provenance`, function()
			local response = { code = code, body = if code == 403 then "The upstream request timed out." else "" }
			local err = Error.fromResponse(response)
			expect(err.type).to.equal(Error.Kind.Timeout)
			expect(err.response).to.equal(response)
			expect(tostring(err):find("timed out", 1, true)).to.be.ok()
		end)
	end

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
