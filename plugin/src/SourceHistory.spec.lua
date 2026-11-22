return function()
	const SourceHistory = require(script.Parent.SourceHistory)

	const function fixture(maxEntries, maxBytes)
		const clock = { Now = 0 }
		const history = SourceHistory.new({
			Clock = function()
				return clock.Now
			end,
			Horizon = 10,
			MaxBytes = maxBytes or 4096,
			MaxEntries = maxEntries or 8,
		})
		return history, clock
	end

	it("only recognizes a proven operation for the same instance generation", function()
		const history = fixture()
		const instance = {}
		history:begin("client/1", { script = { Instance = instance, Revision = 1, Source = "first" } })
		expect(history:classify("script", "client/1", instance, 2)).to.equal("Echo")
		expect(history:classify("script", "client/1", instance, 1)).to.equal("Current")
		expect(history:classify("script", "other/1", instance, 2)).to.equal("Independent")
		expect(history:classify("script", nil, instance, 2)).to.equal("Independent")
		expect(history:classify("script", "client/1", {}, 2)).to.equal("Independent")
		expect(history:classify("other", "client/1", instance, 2)).to.equal("Independent")
		expect(history:classify("script", "client/1", instance, 0)).to.equal("Independent")
	end)

	it("uses age only for pruning completed history and never expires pending intent", function()
		const history, clock = fixture()
		const instance = {}
		history:begin("pending", { script = { Instance = instance, Revision = 1, Source = "draft" } })
		history:begin("complete", { script = { Instance = instance, Revision = 2, Source = "saved" } })
		history:finish("complete")
		clock.Now = 10
		expect(history:classify("script", "complete", instance, 3)).to.equal("Echo")
		clock.Now = 10.001
		expect(history:classify("script", "complete", instance, 3)).to.equal("Independent")
		expect(history:classify("script", "pending", instance, 3)).to.equal("Echo")
	end)

	it("pins acknowledged operations until their publication coverage is consumed", function()
		const history, clock = fixture(1)
		const instance = {}
		history:begin("first", { script = { Instance = instance, Revision = 1, Source = "draft" } })
		history:acknowledge("first", 4)
		clock.Now = 100
		history:consume(3)
		expect(history:classify("script", "first", instance, 2)).to.equal("Echo")
		expect(pcall(function()
			history:begin("second", { script = { Instance = instance, Revision = 2, Source = "newer" } })
		end)).to.equal(false)
		history:consume(4)
		clock.Now = 110
		expect(history:classify("script", "first", instance, 2)).to.equal("Echo")
		clock.Now = 110.001
		expect(history:classify("script", "first", instance, 2)).to.equal("Independent")
	end)

	it("completes coverage regardless of acknowledgement and publication ordering", function()
		for _, acknowledgedFirst in { false, true } do
			const history, clock = fixture()
			const instance = {}
			history:begin("first", { script = { Instance = instance, Revision = 1, Source = "draft" } })
			if acknowledgedFirst then
				history:acknowledge("first", 2)
			end
			history:consume(2)
			clock.Now = 50
			if not acknowledgedFirst then
				history:acknowledge("first", 2)
			end
			expect(history:classify("script", "first", instance, 2)).to.equal(
				if acknowledgedFirst then "Independent" else "Echo"
			)
		end
	end)

	it("backpressures unresolved operations instead of silently evicting them", function()
		const history = fixture(1)
		const instance = {}
		history:begin("first", { script = { Instance = instance, Revision = 1, Source = "a" } })
		expect(pcall(function()
			history:begin("second", { script = { Instance = instance, Revision = 2, Source = "b" } })
		end)).to.equal(false)
		expect(history:classify("script", "first", instance, 2)).to.equal("Echo")
		history:finish("first")
		history:begin("second", { script = { Instance = instance, Revision = 2, Source = "b" } })
		expect(history:classify("script", "first", instance, 2)).to.equal("Independent")
	end)

	it("enforces byte capacity and rejects duplicate operation identities", function()
		const history = fixture(8, 32)
		const instance = {}
		expect(pcall(function()
			history:begin("huge", { script = { Instance = instance, Revision = 1, Source = string.rep("x", 33) } })
		end)).to.equal(false)
		history:begin("a", { script = { Instance = instance, Revision = 1, Source = "x" } })
		history:finish("a")
		expect(pcall(function()
			history:begin(
				"huge-again",
				{ script = { Instance = instance, Revision = 2, Source = string.rep("x", 33) } }
			)
		end)).to.equal(false)
		expect(history:classify("script", "a", instance, 2)).to.equal("Echo")
		expect(pcall(function()
			history:begin("a", { script = { Instance = instance, Revision = 2, Source = "y" } })
		end)).to.equal(false)
	end)

	it("keeps causal decisions correct over one thousand replayable schedules", function()
		for seed = 1, 1000 do
			const history, clock = fixture(64, 65536)
			const instance = {}
			local state = seed
			local revision = 0
			const sent = {}
			for step = 1, 32 do
				state = (state * 48271) % 2147483647
				if state % 3 == 0 then
					revision += 1
				else
					const operation = `client/{seed}/{step}`
					history:begin(operation, { script = { Instance = instance, Revision = revision, Source = "same" } })
					sent[operation] = revision
					if state % 2 == 0 then
						history:finish(operation)
					end
				end
				clock.Now += 0.01
				for operation, baseline in sent do
					assert(
						history:classify("script", operation, instance, revision)
							== (if revision > baseline then "Echo" else "Current"),
						`Causal history failed at seed {seed}, step {step}.`
					)
				end
				assert(
					history:classify("script", "independent", instance, revision) == "Independent",
					`Independent history was misclassified at seed {seed}, step {step}.`
				)
			end
		end
	end)
end
