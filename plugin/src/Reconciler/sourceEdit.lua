--[[

sourceEdit

Builds one UTF-8-safe replacement without touching unchanged editor text. Positions use
ScriptDocument's one-based line numbers and UTF-8 byte columns, not UTF-16 offsets.

--]]

const CHUNK_SIZE = 1024

const function splitBoundary(text, index)
	const byte = string.byte(text, index)
	return (byte ~= nil and byte >= 0x80 and byte < 0xC0) or (byte == 10 and string.byte(text, index - 1) == 13)
end

const function position(text, offset)
	local line, start = 1, 1
	while true do
		const newline = string.find(text, "\n", start, true)
		if newline == nil or newline >= offset then
			return line, offset - start + 1
		end
		line += 1
		start = newline + 1
	end
end

return function(current, target)
	assert(type(current) == "string", "Current Source must be a string.")
	assert(type(target) == "string", "Target Source must be a string.")
	if current == target then
		return nil
	end

	local prefix = 0
	const maximum = math.min(#current, #target)
	while
		prefix + CHUNK_SIZE <= maximum
		and string.sub(current, prefix + 1, prefix + CHUNK_SIZE)
			== string.sub(target, prefix + 1, prefix + CHUNK_SIZE)
	do
		prefix += CHUNK_SIZE
	end
	while prefix < maximum and string.byte(current, prefix + 1) == string.byte(target, prefix + 1) do
		prefix += 1
	end
	while prefix > 0 and (splitBoundary(current, prefix + 1) or splitBoundary(target, prefix + 1)) do
		prefix -= 1
	end

	local suffix = 0
	while
		suffix + CHUNK_SIZE <= maximum - prefix
		and string.sub(current, #current - suffix - CHUNK_SIZE + 1, #current - suffix)
			== string.sub(target, #target - suffix - CHUNK_SIZE + 1, #target - suffix)
	do
		suffix += CHUNK_SIZE
	end
	while
		suffix < maximum - prefix
		and string.byte(current, #current - suffix) == string.byte(target, #target - suffix)
	do
		suffix += 1
	end
	while
		suffix > 0 and (splitBoundary(current, #current - suffix + 1) or splitBoundary(target, #target - suffix + 1))
	do
		suffix -= 1
	end

	const startLine, startCharacter = position(current, prefix + 1)
	const endLine, endCharacter = position(current, #current - suffix + 1)
	return {
		EndCharacter = endCharacter,
		EndLine = endLine,
		StartCharacter = startCharacter,
		StartLine = startLine,
		Text = string.sub(target, prefix + 1, #target - suffix),
	}
end
