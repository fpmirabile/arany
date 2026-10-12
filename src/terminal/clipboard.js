ObjC.import('AppKit');
ObjC.import('CoreServices');

function run(args) {
    if (args.length !== 3) { throw new Error('invalid clipboard request'); }
    const board = args[0] === '' ? $.NSPasteboard.generalPasteboard :
        $.NSPasteboard.pasteboardWithName(args[0]);
    const generation = board.changeCount;
    const types = ObjC.deepUnwrap(board.types) || [];
    if (types.length > 128 || types.some(name => name.length > 256)) {
        throw new Error('clipboard type limit');
    }
    let kind;
    let tag;
    let limit;
    if (types.indexOf('public.png') !== -1) {
        kind = 'public.png';
        tag = 'P\n';
        limit = Number(args[1]);
    } else {
        for (const name of types) {
            if ($.UTTypeConformsTo($(name), $('public.image'))) {
                throw new Error('unsupported clipboard image');
            }
        }
        if (types.indexOf('public.utf8-plain-text') === -1) {
            throw new Error('unsupported clipboard type');
        }
        kind = 'public.utf8-plain-text';
        tag = 'T\n';
        limit = Number(args[2]);
    }
    const data = board.dataForType(kind);
    if (!data || data.length > limit || board.changeCount !== generation) {
        throw new Error('clipboard changed or exceeded limit');
    }
    const output = $.NSFileHandle.fileHandleWithStandardOutput;
    output.writeData($(tag).dataUsingEncoding($.NSUTF8StringEncoding));
    output.writeData(data);
}
