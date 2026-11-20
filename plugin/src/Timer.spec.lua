return function()
	local Settings = require(script.Parent.Settings)
	local Timer = require(script.Parent.Timer)
	local originalEnabled

	beforeEach(function()
		originalEnabled = Settings:get("timingLogsEnabled")
		-- Exercise configuration changes without persisting test values to the plugin store.
		Settings._values.timingLogsEnabled = false
	end)

	afterEach(function()
		while Timer._entries[coroutine.running()] do
			Timer.stop()
		end
		Settings._values.timingLogsEnabled = originalEnabled
	end)

	it("should profile nested scopes even when console timing logs are disabled", function()
		local thread = coroutine.running()
		Timer.start("TimerTest.Outer")
		Timer.start("TimerTest.Inner")
		local entries = Timer._entries[thread]
		expect(#entries).to.equal(2)
		expect(entries[1].Label).to.equal("TimerTest.Outer")
		expect(entries[2].Label).to.equal("TimerTest.Inner")
		expect(entries[1].Start).to.equal(nil)
		expect(entries[2].Start).to.equal(nil)

		Timer.stop()
		expect(#entries).to.equal(1)
		expect(entries[1].Label).to.equal("TimerTest.Outer")
		Timer.stop()
		expect(Timer._entries[thread]).to.equal(nil)
	end)

	it("should reject missing or non-string labels without opening a scope", function()
		for _, invalid in { false, 1, {} } do
			expect(function()
				Timer.start(invalid)
			end).to.throw()
			expect(Timer._entries[coroutine.running()]).to.equal(nil)
		end
		expect(function()
			Timer.start()
		end).to.throw()
		expect(Timer._entries[coroutine.running()]).to.equal(nil)
	end)

	it("should reject an unmatched stop without consuming another coroutine's scope", function()
		local other = coroutine.create(function()
			Timer.start("TimerTest.Other")
			coroutine.yield()
			Timer.stop()
		end)
		local success, err = coroutine.resume(other)
		assert(success, err)
		expect(function()
			Timer.stop()
		end).to.throw()
		expect(Timer._entries[other][1].Label).to.equal("TimerTest.Other")
		success, err = coroutine.resume(other)
		assert(success, err)
		expect(Timer._entries[other]).to.equal(nil)
	end)

	it("should keep interleaved coroutine scopes separate", function()
		for _, enabled in { false, true } do
			Settings._values.timingLogsEnabled = enabled
			local first = coroutine.create(function()
				Timer._start("TimerTest.First")
				coroutine.yield()
				Timer._start("TimerTest.Child")
				Timer._stop()
				Timer._stop()
			end)
			local second = coroutine.create(function()
				Timer.start("TimerTest.Second")
				coroutine.yield()
				Timer.stop()
			end)
			local success, err = coroutine.resume(first)
			assert(success, err)
			success, err = coroutine.resume(second)
			assert(success, err)
			expect(Timer._entries[first][1].Label).to.equal("TimerTest.First")
			expect(Timer._entries[second][1].Label).to.equal("TimerTest.Second")
			expect(Timer._entries[first] == Timer._entries[second]).to.equal(false)

			success, err = coroutine.resume(first)
			assert(success, err)
			expect(Timer._entries[first]).to.equal(nil)
			expect(Timer._entries[second][1].Label).to.equal("TimerTest.Second")
			success, err = coroutine.resume(second)
			assert(success, err)
			expect(Timer._entries[second]).to.equal(nil)
		end
	end)

	it("should retain each scope's logging choice across configuration changes", function()
		for _, enabled in { false, true } do
			Settings._values.timingLogsEnabled = enabled
			Timer.start("TimerTest.BeforeSettingChange")
			Settings._values.timingLogsEnabled = not enabled
			Timer.start("TimerTest.AfterSettingChange")
			local entries = Timer._entries[coroutine.running()]
			expect(entries[1].Start ~= nil).to.equal(enabled)
			expect(entries[2].Start ~= nil).to.equal(not enabled)
			Timer.stop()
			expect(entries[1].Start ~= nil).to.equal(enabled)
			Timer.stop()
			expect(Timer._entries[coroutine.running()]).to.equal(nil)
		end
	end)

	it("should release completed stacks and weakly reference suspended coroutines", function()
		expect(getmetatable(Timer._entries).__mode).to.equal("k")
		Timer.start("TimerTest.Completed")
		Timer.stop()
		expect(Timer._entries[coroutine.running()]).to.equal(nil)
		expect(function()
			Timer.stop()
		end).to.throw()
	end)
end
