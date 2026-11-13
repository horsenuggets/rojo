return function()
	local ApiContext = require(script.Parent.ApiContext)
	local Http = require(script.Parent.Parent.Packages.Http)
	local Promise = require(script.Parent.Parent.Packages.Promise)

	it("should send the captured batch cursor instead of a newer cursor", function()
		local originalPost = Http.post
		local success, err = pcall(function()
			for _, cursor in { 0, 3 } do
				local sentCursor
				Http.post = function(_url, body)
					sentCursor = Http.msgpackDecode(body).messageCursor
					return Promise.resolve({ code = 200, body = Http.msgpackEncode({ sessionId = "current-session" }) })
				end
				local context = ApiContext.new("http://localhost")
				context.__sessionId = "current-session"
				context:setMessageCursor(10)
				local accepted = context:write({ added = {}, removed = {}, updated = {} }, cursor):await()
				expect(accepted).to.equal(true)
				expect(sentCursor).to.equal(cursor)
			end
		end)
		Http.post = originalPost
		assert(success, err)
	end)

	it("should accept write responses without info-level console output", function()
		local Log = require(script.Parent.Parent.Packages.Log)
		local originalInfo = Log.info
		local originalPost = Http.post
		local success, err = pcall(function()
			Log.info = function()
				error("Write responses must not produce info-level console output")
			end
			Http.post = function()
				return Promise.resolve({ code = 200, body = Http.msgpackEncode({ sessionId = "current-session" }) })
			end
			local context = ApiContext.new("http://localhost")
			context.__sessionId = "current-session"
			local accepted, result = context:write({ added = {}, removed = {}, updated = {} }):await()
			expect(accepted).to.equal(true)
			expect(result.sessionId).to.equal("current-session")
		end)
		Log.info = originalInfo
		Http.post = originalPost
		assert(success, err)
	end)

	it("should accept only write responses from the connected session", function()
		local originalPost = Http.post
		local success, err = pcall(function()
			for _, matches in { true, false } do
				local response = {
					sessionId = if matches then "current-session" else "previous-session",
					errors = {},
				}
				Http.post = function()
					return Promise.resolve({ code = 200, body = Http.msgpackEncode(response) })
				end
				local context = ApiContext.new("http://localhost")
				context.__sessionId = "current-session"
				local accepted, result = context:write({ added = {}, removed = {}, updated = {} }):await()
				expect(accepted).to.equal(matches)
				if matches then
					expect(result.sessionId).to.equal("current-session")
				else
					expect(result).to.equal("Server changed ID")
				end
			end
		end)
		Http.post = originalPost
		assert(success, err)
	end)
end
