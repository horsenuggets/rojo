return function()
	local msgpack = require(script.Parent.Parent.Packages.msgpack)
	local Response = require(script.Parent.Parent.Packages.Http.Response)
	local TOKEN_BUDGET = 1024

	local containers = {
		{ name = "fixmap", header = "\x81\x01", map = true },
		{ name = "map16", header = "\xDE\x00\x01\x01", map = true },
		{ name = "map32", header = "\xDF\x00\x00\x00\x01\x01", map = true },
		{ name = "fixarray", header = "\x91" },
		{ name = "array16", header = "\xDC\x00\x01" },
		{ name = "array32", header = "\xDD\x00\x00\x00\x01" },
	}

	local function largeArray(value, count)
		return "\xDD" .. string.pack(">I4", count) .. string.rep(value, count)
	end

	for _, container in containers do
		it(`should share a bounded token budget through nested {container.name} containers`, function()
			local count = 1100
			local message = container.header .. largeArray(container.header .. "\x2A", count)
			local checkpoints = 0
			local result = msgpack.decode(message, function()
				checkpoints += 1
			end)
			local tokens = 1 + (if container.map then 1 else 0) + 1 + count * (if container.map then 3 else 2)
			expect(checkpoints).to.equal(math.floor((tokens - 1) / TOKEN_BUDGET))
			expect(#result[1]).to.equal(count)
			expect(result[1][count][1]).to.equal(42)
		end)
	end

	it("should count tiny empty containers rather than restart their budgets", function()
		local calls = 0
		local value = table.concat({
			"\x80",
			"\xDE\x00\x00",
			"\xDF\x00\x00\x00\x00",
			"\x90",
			"\xDC\x00\x00",
			"\xDD\x00\x00\x00\x00",
		})
		local message = "\xDD" .. string.pack(">I4", 6 * TOKEN_BUDGET) .. string.rep(value, TOKEN_BUDGET)
		local result = msgpack.decode(message, function()
			calls += 1
		end)
		expect(calls).to.equal(6)
		expect(#result).to.equal(6 * TOKEN_BUDGET)
		expect(next(result[#result])).to.equal(nil)
	end)

	it("should suspend without completing and keep interleaved decode budgets independent", function()
		local message = largeArray("\x91\x2A", 1100)
		local completions = 0
		local calls = { 0, 0 }
		local threads = {}
		for index = 1, 2 do
			threads[index] = coroutine.create(function()
				local result = msgpack.decode(message, function()
					calls[index] += 1
					coroutine.yield("checkpoint")
				end)
				expect(result[1100][1]).to.equal(42)
				completions += 1
			end)
		end
		for _ = 1, 2 do
			for _, thread in threads do
				local ok, marker = coroutine.resume(thread)
				expect(ok).to.equal(true)
				expect(marker).to.equal("checkpoint")
				expect(coroutine.status(thread)).to.equal("suspended")
				expect(completions).to.equal(0)
			end
		end
		for index, thread in threads do
			expect(coroutine.resume(thread)).to.equal(true)
			expect(coroutine.status(thread)).to.equal("dead")
			expect(calls[index]).to.equal(2)
		end
		expect(completions).to.equal(2)
	end)

	it("should forward a response checkpoint and propagate its abort without further parsing", function()
		local sentinel = {}
		local calls = 0
		local response = Response.fromRobloxResponse({
			StatusCode = 200,
			Body = largeArray("\x00", TOKEN_BUDGET) .. "\xC1",
			Headers = {},
		})
		local ok, problem = pcall(Response.msgpack, response, function()
			calls += 1
			error(sentinel)
		end)
		expect(ok).to.equal(false)
		expect(problem).to.equal(sentinel)
		expect(calls).to.equal(1)
		expect(Response.msgpack(Response.fromRobloxResponse({ StatusCode = 200, Body = "\x2A", Headers = {} }))).to.equal(
			42
		)
	end)

	it("should preserve numeric binary extension and nil contracts across checkpoints and repeated decodes", function()
		local cases = {
			"\xC0",
			"\xC2",
			"\xC3",
			"\x00",
			"\xFF",
			"\xCC\xFF",
			"\xCD\x12\x34",
			"\xCE\x12\x34\x56\x78",
			"\xD0\x80",
			"\xD1\x80\x00",
			"\xD2\x80\x00\x00\x00",
			"\xCA\x7F\x80\x00\x00",
			"\xCA\xFF\x80\x00\x00",
			"\xCA\x7F\x80\x00\x01",
			"\xCB\x3F\xF8\x00\x00\x00\x00\x00\x00",
			"\xCB\x7F\xF8\x00\x00\x00\x00\x00\x00",
			"\xCF\x12\x34\x56\x78\x87\x65\x43\x21",
			"\xD3\xFF\xFF\xFF\xFF\xFF\xFF\xFF\xFE",
			"\xC4\x02\x00\xFF",
			"\xC5\x00\x02\x00\xFF",
			"\xC6\x00\x00\x00\x02\x00\xFF",
			"\xD4\x07\xFF",
			"\xD5\x07" .. "ab",
			"\xD6\x07" .. "abcd",
			"\xD7\x07" .. "abcdefgh",
			"\xD8\x07" .. "abcdefghijklmnop",
			"\xC7\x03\x07" .. "abc",
			"\xC8\x00\x03\x07" .. "abc",
			"\xC9\x00\x00\x00\x03\x07" .. "abc",
		}
		for _, wire in cases do
			local expected = msgpack.encode(msgpack.decode(wire))
			local message = "\xDD" .. string.pack(">I4", TOKEN_BUDGET + 1) .. string.rep("\xC0", TOKEN_BUDGET) .. wire
			for _ = 1, 2 do
				local calls = 0
				local decoded = msgpack.decode(message, function()
					calls += 1
				end)
				expect(calls).to.equal(1)
				expect(msgpack.encode(decoded[TOKEN_BUDGET + 1])).to.equal(expected)
			end
		end
	end)

	it("should preserve malformed input and trailing-byte contracts after failed or aborted decodes", function()
		for _, wire in
			{
				"",
				"\xC1",
				"\x81",
				"\xD9",
				"\xCD\x01",
				"\xDC\x00",
				"\xDF\x00\x00\x00\x01",
				"\xDD" .. string.pack(">I4", TOKEN_BUDGET + 1) .. string.rep("\x00", TOKEN_BUDGET) .. "\xC1",
			}
		do
			expect(pcall(msgpack.decode, wire)).to.equal(false)
			expect(pcall(msgpack.decode, wire, function() end)).to.equal(false)
			expect(msgpack.decode("\xCD\x12\x34")).to.equal(0x1234)
		end
		local calls = 0
		expect(msgpack.decode("\x2A\xC1", function()
			calls += 1
		end)).to.equal(42)
		expect(calls).to.equal(0)
	end)
end
