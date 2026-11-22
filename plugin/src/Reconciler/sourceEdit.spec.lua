--[[

sourceEdit.spec

Checks exact reconstruction and valid UTF-8 edit boundaries independently of editor I/O.

--]]

return function()
	const sourceEdit = require("./sourceEdit")

	const function offset(text, line, character)
		local cursor = 1
		for _ = 2, line do
			cursor = assert(string.find(text, "\n", cursor, true)) + 1
		end
		return cursor + character - 1
	end

	it("reconstructs every pair of representative sources without splitting UTF-8", function()
		const sources = {
			"",
			"a",
			"ab",
			"a\nb",
			"\n",
			"a\r\nb\r\n",
			"a\n",
			"a\r\n",
			"a\rb",
			"é",
			"ê",
			"è",
			"\u{128}",
			"界",
			"\u{1F642}",
			"\u{1F643}",
			"\u{1F682}",
			"aé界\u{1F642}\nb",
			"aê界\u{1F643}\nb",
		}
		for _, current in sources do
			for _, target in sources do
				const edit = sourceEdit(current, target)
				if current == target then
					expect(edit).to.equal(nil)
				else
					const first = offset(current, edit.StartLine, edit.StartCharacter)
					const last = offset(current, edit.EndLine, edit.EndCharacter)
					const prefix = string.sub(current, 1, first - 1)
					const suffix = string.sub(current, last)
					assert(utf8.len(prefix) ~= nil, "The prefix must end at a UTF-8 boundary.")
					assert(utf8.len(edit.Text) ~= nil, "The replacement must contain complete UTF-8 characters.")
					assert(utf8.len(suffix) ~= nil, "The suffix must start at a UTF-8 boundary.")
					expect(prefix .. edit.Text .. suffix).to.equal(target)
					const repeated = sourceEdit(current, target)
					for key, value in edit do
						expect(repeated[key]).to.equal(value)
					end
				end
			end
		end
	end)

	it("leaves unchanged lines outside the single replacement", function()
		const edit = sourceEdit('require("Short")\nreturn value\n-- draft', 'require("Longer")\nreturn value\n-- draft')
		expect(edit.StartLine).to.equal(1)
		expect(edit.EndLine).to.equal(1)
		expect(edit.StartCharacter).to.equal(10)
		expect(edit.EndCharacter).to.equal(15)
		expect(edit.Text).to.equal("Longer")
	end)

	it("keeps UTF-8 boundaries across large equal prefix and suffix chunks", function()
		for padding = 1022, 1026 do
			const prefix = string.rep("a", padding)
			const suffix = string.rep("\u{1F642}", padding)
			const edit = sourceEdit(prefix .. "é" .. suffix, prefix .. "ê" .. suffix)
			expect(edit.StartCharacter).to.equal(#prefix + 1)
			expect(edit.EndCharacter).to.equal(#prefix + #"é" + 1)
			expect(edit.Text).to.equal("ê")
		end
	end)

	it("finds a small replacement in a large multiline source", function()
		const prefix = string.rep("-- prefix\n", 50000)
		const suffix = string.rep("\n-- suffix", 50000)
		const edit = sourceEdit(prefix .. "before" .. suffix, prefix .. "after" .. suffix)
		expect(edit.StartLine).to.equal(50001)
		expect(edit.EndLine).to.equal(50001)
		expect(edit.StartCharacter).to.equal(1)
		expect(edit.EndCharacter).to.equal(7)
		expect(edit.Text).to.equal("after")
	end)

	it("keeps CRLF together so editor newline normalization cannot duplicate a boundary", function()
		const edit = sourceEdit("a\n", "a\r\n")
		expect(edit.StartLine).to.equal(1)
		expect(edit.StartCharacter).to.equal(2)
		expect(edit.EndLine).to.equal(2)
		expect(edit.EndCharacter).to.equal(1)
		expect(edit.Text).to.equal("\r\n")
	end)

	it("maps multiline insertion and deletion using byte columns", function()
		const insertion = sourceEdit("é\n界", "é\n\u{1F642}\n界")
		expect(insertion.StartLine).to.equal(2)
		expect(insertion.StartCharacter).to.equal(1)
		expect(insertion.EndLine).to.equal(2)
		expect(insertion.EndCharacter).to.equal(1)
		expect(insertion.Text).to.equal("\u{1F642}\n")
		const deletion = sourceEdit("é\n\u{1F642}\n界", "é\n界")
		expect(deletion.StartLine).to.equal(2)
		expect(deletion.EndLine).to.equal(3)
		expect(deletion.Text).to.equal("")
	end)
end
